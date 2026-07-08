//! Markdown renderer — the public seam replacing
//! `gpui_component::text::TextView::markdown`.
//!
//! Drop-in replacement with per-block layout control: code blocks get
//! `overflow_x_scroll` + a line-number gutter, and streaming bodies paint
//! plain + cursor without re-parsing completed blocks.
//!
//! Architecture: `Markdown::into_element` parses mdast once, maps it to manox
//! `Block`s, and renders each as a `div` + `StyledText::with_highlights`
//! composition. `with_highlights` overlays `(Range, HighlightStyle)` on top of
//! the base font/color that `StyledText` inherits from `window.text_style()`
//! (set by the parent `div`'s `.text_sm()`/`.text_color()`/…) at layout time,
//! so the renderer never constructs a `TextStyle` or `impl`s `Element`.

pub mod ast;
pub mod theme;

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    AnyElement, ElementId, HighlightStyle, Hsla, IntoElement, SharedString, StyledText, div, px,
};
use gpui_component::highlighter::SyntaxHighlighter;
use gpui_component::{Theme, h_flex, v_flex};
use ropey::Rope;

use crate::markdown::ast::{Block, InlineRuns, ListItem, TableAlign};
use crate::markdown::theme::MdStyles;

/// Markdown document renderer.
pub struct Markdown {
    id: ElementId,
    text: SharedString,
    styles: Option<MdStyles>,
    scrollable: bool,
    streaming: bool,
}

impl Markdown {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            styles: None,
            scrollable: false,
            streaming: false,
        }
    }

    /// Bridge the workspace theme (colors + syntax highlight palette) into the
    /// renderer's style table.
    pub fn theme(mut self, theme: &Theme) -> Self {
        self.styles = Some(MdStyles::from_theme(theme));
        self
    }

    /// Mount an internal vertical scrollbar. When enabled the renderer sizes
    /// to its parent's box, so the parent must carry a fixed height.
    pub fn scrollable(mut self, scrollable: bool) -> Self {
        self.scrollable = scrollable;
        self
    }

    /// Cross-block text selection + Cmd+C copy. Currently a no-op — the
    /// selection layer lands in a follow-up; per-block copy buttons remain.
    pub fn selectable(self, _selectable: bool) -> Self {
        self
    }

    /// Mark the document as mid-stream: the body renders plain + a trailing
    /// cursor, and the full markdown layout mounts once when the stream ends.
    pub fn streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }
}

impl IntoElement for Markdown {
    type Element = AnyElement;

    fn into_element(self) -> Self::Element {
        let id = self.id;
        let Some(styles) = self.styles else {
            return div().id(id).into_any_element();
        };

        if self.streaming {
            // Plain text + cursor while streaming — no mdast re-parse per token
            // delta, so the body never reflows mid-stream.
            let shown = format!("{}▌", self.text);
            return div()
                .id(id)
                .w_full()
                .min_w_0()
                .overflow_hidden()
                .text_sm()
                .child(SharedString::from(shown))
                .into_any_element();
        }

        let blocks = ast::parse(&self.text, &styles);
        let mut col = v_flex().id(id).w_full().min_w_0().gap_2();
        col = if self.scrollable {
            col.h_full().overflow_y_scroll()
        } else {
            col.overflow_hidden()
        };
        for (i, block) in blocks.into_iter().enumerate() {
            col = col.child(render_block(block, &styles, i));
        }
        col.into_any_element()
    }
}

fn render_block(block: Block, styles: &MdStyles, idx: usize) -> AnyElement {
    match block {
        Block::Paragraph(runs) => paragraph(&runs.text, &runs.highlights),
        Block::Heading { runs, depth } => heading(&runs.text, &runs.highlights, depth),
        Block::Code { lang, value } => code_block(&value, lang.as_deref(), styles, idx),
        Block::Diff { value } => diff_block(&value, styles, idx),
        Block::Blockquote(inner) => blockquote(inner, styles),
        Block::List { ordered, items } => list_block(ordered, items, styles),
        Block::Table { rows, align } => table_block(rows, align, styles, idx),
        Block::ThematicBreak => div()
            .w_full()
            .h(px(1.))
            .bg(styles.border)
            .into_any_element(),
    }
}

fn paragraph(text: &str, highlights: &[(Range<usize>, HighlightStyle)]) -> AnyElement {
    div()
        .w_full()
        .min_w_0()
        .text_sm()
        .child(
            StyledText::new(SharedString::from(text.to_string()))
                .with_highlights(highlights.iter().cloned()),
        )
        .into_any_element()
}

/// Heading: bold (set in `ast::inline_of`) scaled by depth so H1–H3 grow and
/// H4–H6 fall back to body size + bold. The base color is inherited from the
/// parent div — same contract as `paragraph`, so streaming→finalized never
/// recolors.
fn heading(text: &str, highlights: &[(Range<usize>, HighlightStyle)], depth: u8) -> AnyElement {
    let mut d = div().w_full().min_w_0().child(
        StyledText::new(SharedString::from(text.to_string()))
            .with_highlights(highlights.iter().cloned()),
    );
    d = match depth {
        1 => d.text_xl(),
        2 => d.text_lg(),
        3 => d.text_base(),
        _ => d.text_sm(),
    };
    d.into_any_element()
}

/// Code block: line-number gutter (fixed during horizontal scroll) + a
/// horizontally-scrollable, non-wrapping code run with tree-sitter syntax
/// highlighting via `SyntaxHighlighter`.
fn code_block(value: &str, lang: Option<&str>, styles: &MdStyles, idx: usize) -> AnyElement {
    let highlights = code_highlights(value, lang, styles);
    let line_count = value.split('\n').count().max(1);
    let gutter: String = (1..=line_count).map(|n| format!("{n:>3}\n")).collect();
    let gutter = gutter.trim_end_matches('\n');

    h_flex()
        .w_full()
        .min_w_0()
        .rounded_md()
        .bg(styles.secondary)
        .overflow_hidden()
        .child(
            div()
                .py_3()
                .px_2()
                .text_xs()
                .text_color(styles.muted)
                .whitespace_nowrap()
                .child(SharedString::from(gutter.to_string())),
        )
        .child(
            div()
                .id(("code", idx))
                .flex_1()
                .min_w_0()
                .overflow_x_scroll()
                .py_3()
                .px_3()
                .text_xs()
                .text_color(styles.foreground)
                .whitespace_nowrap()
                .child(
                    StyledText::new(SharedString::from(value.to_string()))
                        .with_highlights(highlights.iter().cloned()),
                ),
        )
        .into_any_element()
}

/// Per-frame syntax highlighting for a code run. The highlighter is built and
/// parsed fresh each render; tree-sitter parses a few KB in well under a
/// millisecond, so this is correct before it is cached. Lives on the render
/// thread — `SyntaxHighlighter` owns a thread-local `Parser` and is not `Send`.
fn code_highlights(
    value: &str,
    lang: Option<&str>,
    styles: &MdStyles,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let rope = Rope::from_str(value);
    let mut hl = SyntaxHighlighter::new(lang.unwrap_or("text"));
    let _ = hl.update(None, &rope, None);
    hl.styles(&(0..value.len()), styles.highlight_theme.as_ref())
}

/// Unified-diff block. Each line carries its own wash + a 2px left bar in the
/// accent color; `+`/`-` prefixes are kept (TUI convention) and re-tinted to
/// the accent. No line-number gutter — hunk `@@ -a,b +c,d @@` makes per-line
/// numbers misleading. Horizontal scroll keeps long added/removed runs aligned.
fn diff_block(value: &str, styles: &MdStyles, idx: usize) -> AnyElement {
    let mut inner = v_flex().min_w_0();
    for line in value.lines() {
        let (bg, bar, fg) = classify_diff_line(line, styles);
        inner = inner.child(
            div()
                .border_l_2()
                .border_color(bar)
                .bg(bg)
                .px_3()
                .py(px(1.))
                .text_xs()
                .text_color(fg)
                .whitespace_nowrap()
                .child(SharedString::from(line.to_string())),
        );
    }
    div()
        .w_full()
        .min_w_0()
        .rounded_md()
        .overflow_hidden()
        .border_1()
        .border_color(styles.border)
        .bg(styles.secondary)
        .child(
            div()
                .id(("diff-scroll", idx))
                .w_full()
                .min_w_0()
                .overflow_x_scroll()
                .child(inner),
        )
        .into_any_element()
}

/// Classify a unified-diff line into (background, left_bar, foreground).
///
/// Metadata lines (file headers `+++ `/`--- `, `diff `/`index ` summaries,
/// `@@` hunk markers, `\ No newline` sentinels) are muted. Content lines key
/// on their first byte: a `+`/`-` content line whose text itself starts with
/// `--`/`++` (e.g. a removed `--x` rendering as `---x`) must not be mistaken
/// for a file header — hence the trailing-space anchor on `+++ `/`--- `.
fn classify_diff_line(line: &str, styles: &MdStyles) -> (Hsla, Hsla, Hsla) {
    if line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("@@")
        || line.starts_with("+++ ")
        || line.starts_with("--- ")
        || line.starts_with("\\ ")
    {
        return (styles.secondary, styles.muted, styles.muted);
    }
    if line.starts_with('+') {
        (styles.diff_add_bg, styles.diff_add_fg, styles.diff_add_fg)
    } else if line.starts_with('-') {
        (styles.diff_del_bg, styles.diff_del_fg, styles.diff_del_fg)
    } else {
        (styles.secondary, styles.muted, styles.foreground)
    }
}

fn blockquote(inner: Vec<Block>, styles: &MdStyles) -> AnyElement {
    let mut col = v_flex()
        .w_full()
        .min_w_0()
        .border_l_2()
        .border_color(styles.border)
        .pl_3()
        .gap_2();
    for (i, block) in inner.into_iter().enumerate() {
        col = col.child(render_block(block, styles, i));
    }
    col.into_any_element()
}

/// GFM table. The header row carries a secondary wash + muted text; each cell
/// follows its column's `TableAlign`. A per-column `min_w` floor keeps wide
/// tables from collapsing into wrap-chaos; `items_start` defeats cross-axis
/// stretch so rows keep their natural width and overflow horizontally into
/// the scroll viewport — mirroring the code-block contract, never clipping.
fn table_block(
    rows: Vec<Vec<InlineRuns>>,
    align: Vec<TableAlign>,
    styles: &MdStyles,
    idx: usize,
) -> AnyElement {
    let mut scroll = v_flex()
        .id(("table", idx))
        .w_full()
        .min_w_0()
        .items_start()
        .overflow_x_scroll();
    for (r, row) in rows.into_iter().enumerate() {
        let is_header = r == 0;
        let mut row_flex = h_flex().min_w_0();
        for (c, cell) in row.into_iter().enumerate() {
            let mut cell_div = div()
                .flex_1()
                .min_w(px(140.))
                .px_3()
                .py_2()
                .text_xs()
                .child(
                    StyledText::new(SharedString::from(cell.text))
                        .with_highlights(cell.highlights.iter().cloned()),
                );
            cell_div = match align.get(c).copied().unwrap_or_default() {
                TableAlign::Center => cell_div.text_center(),
                TableAlign::Right => cell_div.text_right(),
                _ => cell_div.text_left(),
            };
            cell_div = if is_header {
                cell_div.bg(styles.secondary).text_color(styles.muted)
            } else {
                cell_div.text_color(styles.foreground)
            };
            row_flex = row_flex.child(cell_div);
        }
        scroll = scroll.child(row_flex);
    }
    div()
        .w_full()
        .min_w_0()
        .rounded_md()
        .overflow_hidden()
        .border_1()
        .border_color(styles.border)
        .child(scroll)
        .into_any_element()
}

fn list_block(ordered: bool, items: Vec<ListItem>, styles: &MdStyles) -> AnyElement {
    let mut col = v_flex().w_full().min_w_0().gap_1();
    for (i, item) in items.into_iter().enumerate() {
        let mut item_col = v_flex().flex_1().min_w_0().gap_1();
        for (j, b) in item.blocks.into_iter().enumerate() {
            item_col = item_col.child(render_block(b, styles, j));
        }
        col = col.child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .child(
                    div()
                        .w(px(16.))
                        .text_sm()
                        .text_color(match item.checked {
                            Some(true) => styles.diff_add_fg,
                            _ => styles.muted,
                        })
                        .child(match item.checked {
                            Some(true) => SharedString::from("✓"),
                            Some(false) => SharedString::from("☐"),
                            None => SharedString::from(if ordered {
                                format!("{}. ", i + 1)
                            } else {
                                "• ".to_string()
                            }),
                        }),
                )
                .child(item_col),
        );
    }
    col.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::Theme;

    fn styles() -> MdStyles {
        MdStyles::from_theme(&Theme::default())
    }

    #[test]
    fn diff_content_with_sign_prefix_not_header() {
        let s = styles();
        // A removed line whose content starts with `-` (whole line `---x`)
        // must classify as removed, not as a `--- a/file` header; same for
        // an added line whose content starts with `+` (`+++x`).
        assert_eq!(classify_diff_line("---x", &s).2, s.diff_del_fg);
        assert_eq!(classify_diff_line("+++x", &s).2, s.diff_add_fg);
    }

    #[test]
    fn diff_metadata_lines_are_muted() {
        let s = styles();
        for line in [
            "--- a/file",
            "+++ b/file",
            "diff --git a/x b/y",
            "index 1234567..abcdefg 100644",
            "@@ -1,2 +1,2 @@",
            "\\ No newline at end of file",
        ] {
            assert_eq!(
                classify_diff_line(line, &s).2,
                s.muted,
                "line {line:?} should be muted metadata"
            );
        }
    }

    #[test]
    fn diff_content_lines_carry_accent() {
        let s = styles();
        assert_eq!(classify_diff_line("+added", &s).2, s.diff_add_fg);
        assert_eq!(classify_diff_line("-removed", &s).2, s.diff_del_fg);
        assert_eq!(classify_diff_line(" context", &s).2, s.foreground);
    }
}
