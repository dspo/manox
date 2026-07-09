//! mdast → manox `Block` model. The sole file that touches the `markdown` crate.
//!
//! Inline children are flattened to a single `String` + a list of
//! `(Range<usize>, HighlightStyle)` overlays at parse time, so per-frame
//! rendering is just `StyledText::with_highlights` — no re-walking the AST.

use std::ops::Range;

use gpui::{FontStyle, FontWeight, HighlightStyle};
use markdown::mdast::{AlignKind, Node};
use markdown::{ParseOptions, to_mdast};

use crate::markdown::theme::MdStyles;

/// A contiguous run of inline text with style overlays on top of the base
/// font/color (which `StyledText` inherits from `window.text_style()`).
#[derive(Clone, Default)]
pub struct InlineRuns {
    pub text: String,
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
}

/// Column alignment for table cells. The manox-owned mirror of mdast's
/// `AlignKind` so the renderer does not depend on the `markdown` crate.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum TableAlign {
    #[default]
    None,
    Left,
    Center,
    Right,
}

/// A list item: its `checked` flag carries GFM task-list state (`[ ]`/`[x]`),
/// `None` for plain bullets/numbers.
#[derive(Clone)]
pub struct ListItem {
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
}

/// A block-level node in the manox-owned document model.
#[derive(Clone)]
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
pub fn parse(src: &str, styles: &MdStyles) -> Vec<Block> {
    let opts = ParseOptions::gfm();
    match to_mdast(src, &opts) {
        Ok(Node::Root(root)) => root
            .children
            .iter()
            .filter_map(|n| block_of(n, styles))
            .collect(),
        _ => Vec::new(),
    }
}

fn block_of(node: &Node, styles: &MdStyles) -> Option<Block> {
    match node {
        Node::Paragraph(p) => Some(Block::Paragraph(inline_of(
            &p.children,
            styles,
            false,
            false,
        ))),
        Node::Heading(h) => Some(Block::Heading {
            depth: h.depth,
            runs: inline_of(&h.children, styles, true, false),
        }),
        Node::Code(c) => {
            if c.lang.as_deref() == Some("diff") {
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
            b.children
                .iter()
                .filter_map(|n| block_of(n, styles))
                .collect(),
        )),
        Node::List(l) => {
            let items = l
                .children
                .iter()
                .filter_map(|n| match n {
                    Node::ListItem(li) => Some(ListItem {
                        checked: li.checked,
                        blocks: li
                            .children
                            .iter()
                            .filter_map(|n| block_of(n, styles))
                            .collect(),
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
                                Node::TableCell(tc) => {
                                    inline_of(&tc.children, styles, false, false)
                                }
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

fn inline_of(children: &[Node], styles: &MdStyles, bold: bool, italic: bool) -> InlineRuns {
    let mut runs = InlineRuns::default();
    let active = ActiveStyle {
        bold,
        italic,
        strikethrough: false,
        code: false,
    };
    collect_inline(children, styles, active, &mut runs);
    runs
}

/// Cumulative inline formatting at the current recursion depth. One range is
/// emitted per `Text`/`InlineCode` segment carrying the *combined* style of
/// every active overlay, so the resulting ranges are non-overlapping and
/// sorted — `StyledText::compute_runs` requires both, and overlapping ranges
/// (one per formatting node) misalign run lengths and slice mid-codepoint.
#[derive(Clone, Copy, Default)]
struct ActiveStyle {
    bold: bool,
    italic: bool,
    strikethrough: bool,
    code: bool,
}

impl ActiveStyle {
    /// Build the highlight for the current segment. Returns `None` when no
    /// overlay is active so plain text stays a single base-style run.
    fn highlight(&self, styles: &MdStyles) -> Option<HighlightStyle> {
        let hs = HighlightStyle {
            font_weight: self.bold.then_some(FontWeight::BOLD),
            font_style: self.italic.then_some(FontStyle::Italic),
            background_color: self.code.then_some(styles.secondary),
            strikethrough: self.strikethrough.then_some(gpui::StrikethroughStyle::default()),
            ..Default::default()
        };
        (hs != HighlightStyle::default()).then_some(hs)
    }
}

fn collect_inline(
    children: &[Node],
    styles: &MdStyles,
    active: ActiveStyle,
    runs: &mut InlineRuns,
) {
    for node in children {
        match node {
            Node::Text(t) => {
                let start = runs.text.len();
                runs.text.push_str(&t.value);
                let end = runs.text.len();
                if let Some(hs) = active.highlight(styles) {
                    runs.highlights.push((start..end, hs));
                }
            }
            Node::Strong(s) => {
                let mut a = active;
                a.bold = true;
                collect_inline(&s.children, styles, a, runs);
            }
            Node::Emphasis(e) => {
                let mut a = active;
                a.italic = true;
                collect_inline(&e.children, styles, a, runs);
            }
            Node::InlineCode(c) => {
                // `InlineCode.value` is literal text, not inline children; it
                // carries the code wash and inherits any surrounding emphasis.
                let mut a = active;
                a.code = true;
                let start = runs.text.len();
                runs.text.push_str(&c.value);
                let end = runs.text.len();
                if let Some(hs) = a.highlight(styles) {
                    runs.highlights.push((start..end, hs));
                }
            }
            Node::Delete(d) => {
                let mut a = active;
                a.strikethrough = true;
                collect_inline(&d.children, styles, a, runs);
            }
            Node::Link(l) => {
                // Link text inherits surrounding style; underline/color arrives
                // with the selection layer (which owns hovered-link state).
                collect_inline(&l.children, styles, active, runs);
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
    use gpui_component::Theme;

    fn styles() -> MdStyles {
        MdStyles::from_theme(&Theme::default())
    }

    #[test]
    fn parses_paragraph_and_heading() {
        let s = styles();
        let blocks = parse("# Title\n\nbody text", &s);
        assert!(matches!(
            blocks.first(),
            Some(Block::Heading { depth: 1, .. })
        ));
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn parses_code_block_with_lang() {
        let s = styles();
        let blocks = parse("```rust\nfn main() {}\n```", &s);
        let (lang, value) = match blocks.first() {
            Some(Block::Code { lang, value }) => (lang.clone(), value.clone()),
            _ => panic!("expected code block"),
        };
        assert_eq!(lang.as_deref(), Some("rust"));
        assert!(value.contains("fn main"));
    }

    #[test]
    fn parses_list_ordered_and_unordered() {
        let s = styles();
        assert!(matches!(
            parse("- a\n- b", &s).first(),
            Some(Block::List { ordered: false, .. })
        ));
        assert!(matches!(
            parse("1. a\n2. b", &s).first(),
            Some(Block::List { ordered: true, .. })
        ));
    }

    #[test]
    fn inline_collects_bold_italic_code_overlays() {
        let s = styles();
        let blocks = parse("plain **bold** *it* `code`", &s);
        let runs = match blocks.first() {
            Some(Block::Paragraph(r)) => r.clone(),
            _ => panic!("expected paragraph"),
        };
        assert!(runs.text.contains("bold"));
        assert!(runs.text.contains("code"));
        assert!(!runs.highlights.is_empty());
    }

    #[test]
    fn blockquote_nests_children() {
        let s = styles();
        let blocks = parse("> quoted\n> more", &s);
        match blocks.first() {
            Some(Block::Blockquote(inner)) => assert!(!inner.is_empty()),
            _ => panic!("expected blockquote"),
        }
    }

    #[test]
    fn diff_lang_routes_to_diff_block() {
        let s = styles();
        let blocks = parse("```diff\n+added\n-removed\n```", &s);
        match blocks.first() {
            Some(Block::Diff { value }) => {
                assert!(value.contains("+added"));
                assert!(value.contains("-removed"));
            }
            _ => panic!("expected diff block"),
        }
    }

    #[test]
    fn parses_table_rows() {
        let s = styles();
        let blocks = parse("| a | b |\n| --- | --- |\n| 1 | 2 |\n", &s);
        match blocks.first() {
            // mdast consumes the delimiter row as alignment metadata, so
            // children are header + one body row.
            Some(Block::Table { rows, .. }) => assert_eq!(rows.len(), 2),
            _ => panic!("expected table"),
        }
    }

    #[test]
    fn task_list_carries_checked_state() {
        let s = styles();
        let blocks = parse("- [x] done\n- [ ] todo\n", &s);
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
    /// non-overlapping range per text segment. Overlapping ranges misalign
    /// `StyledText::compute_runs` run lengths and slice mid-codepoint — the
    /// ` 粗体中的删除` panic.
    #[test]
    fn nested_inline_formats_emit_non_overlapping_ranges() {
        let s = styles();
        let blocks = parse("**~~粗体中的删除~~**", &s);
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
}
