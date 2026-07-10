//! Markdown renderer — the public seam replacing
//! `gpui_component::text::TextView::markdown`.
//!
//! Drop-in replacement with per-block layout control: code blocks get
//! `overflow_x_scroll` + a line-number gutter, and streaming bodies paint
//! plain + cursor without re-parsing completed blocks.
//!
//! Architecture: `Markdown::into_element` parses mdast once, maps it to manox
//! `Block`s, and renders each as a `div` + `RichText` composition. `RichText`
//! composes `StyledText` for shaping/glyph-painting and overlays rounded
//! inline-code washes + (for code/diff blocks) mouse selection + Cmd/Ctrl+C
//! copy. The base font/color is inherited from `window.text_style()` (set by
//! the parent `div`'s `.text_sm()`/`.text_color()`/…) at layout time, so the
//! renderer never constructs a `TextStyle`.

pub mod ast;
pub mod incremental;
pub mod rich_text;
pub mod theme;

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClipboardItem, ElementId, FontWeight, HighlightStyle, Hsla, IntoElement,
    Pixels, SharedString, div, px,
};
use gpui_component::highlighter::SyntaxHighlighter;
use gpui_component::{
    IconName, Sizable, Theme,
    button::{Button, ButtonVariants},
    h_flex, v_flex,
};
use ropey::Rope;

use crate::markdown::ast::{Block, InlineRuns, ListItem, TableAlign};
use crate::markdown::rich_text::{CodeSpan, RichText};
use crate::markdown::theme::MdStyles;

// Per-(language, content, highlight theme) highlight cache. The message list
// re-renders every dirty frame (a streaming delta dirties the workspace, and
// every frozen block re-paints with it); without this cache each frame would
// rebuild a `SyntaxHighlighter`, re-parse, and re-query styles for *every*
// code block — including completed ones whose content never changes. The
// highlight theme pointer is part of the key so a theme swap invalidates the
// cache automatically; manox has no runtime theme switch today, so in
// practice the key reduces to `(lang, content_hash)`.
//
// `transient` marks an entry written while rendering a still-streaming tail
// block — its content is incomplete and will change on the next delta, so the
// L2 cache write is skipped to avoid polluting the cache with throwaway
// highlight data.
type HighlightCache =
    HashMap<(String, u64, usize), (Vec<(Range<usize>, HighlightStyle)>, bool)>;

thread_local! {
    static CODE_HL_CACHE: RefCell<HighlightCache> = RefCell::new(HashMap::new());
}

/// Markdown document renderer.
pub struct Markdown {
    id: ElementId,
    text: SharedString,
    /// Pre-parsed blocks. When set, `into_element` renders these directly;
    /// otherwise it falls back to `ast::parse(&text)`. The streaming path
    /// (caller supplies blocks from an `IncrementalParser`) and the static
    /// path (editor preview, `Markdown::new`) share the same render pipeline.
    blocks: Option<Arc<Vec<Block>>>,
    styles: Option<MdStyles>,
    scrollable: bool,
    streaming: bool,
    heading_mode: HeadingMode,
}

impl Markdown {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            blocks: None,
            styles: None,
            scrollable: false,
            streaming: false,
            heading_mode: HeadingMode::default(),
        }
    }

    /// Render already-parsed blocks. The caller owns parsing (full or
    /// incremental); `text` is retained only for the streaming cursor and the
    /// copy-button payload. Streaming bodies pass blocks from an
    /// `IncrementalParser` so completed blocks render as formatted markdown
    /// while the tail still grows.
    pub fn blocks(
        id: impl Into<ElementId>,
        text: impl Into<SharedString>,
        blocks: Arc<Vec<Block>>,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            blocks: Some(blocks),
            styles: None,
            scrollable: false,
            streaming: false,
            heading_mode: HeadingMode::default(),
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

    /// Cross-block text selection + Cmd+C copy. Currently a no-op at the
    /// document level — per-block selection lives inside `RichText` mounts
    /// (code/diff blocks are selectable by default); this toggle is reserved
    /// for a future cross-block selection layer.
    pub fn selectable(self, _selectable: bool) -> Self {
        self
    }

    /// Override the inline-code pill wash + corner radius. Without this the
    /// renderer falls back to `theme.secondary` / `theme.radius`.
    pub fn inline_code(mut self, bg: Hsla, radius: Pixels) -> Self {
        if let Some(styles) = &mut self.styles {
            styles.inline_code_bg = bg;
            styles.inline_code_radius = radius;
        }
        self
    }

    /// Mark the document as mid-stream: a trailing cursor `▌` is appended to
    /// the last block's text. The full markdown layout is rendered throughout
    /// streaming (from caller-supplied blocks or a fallback `ast::parse`);
    /// `streaming` no longer gates a plain-text fast-path.
    pub fn streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }

    /// How headings map depth to style. `Scaled` (default) grows the font with
    /// depth; `Uniform` holds every heading at body size and discriminates
    /// levels by weight + decoration, for dense mounts like the message list
    /// where enlarged heading text would drown the body.
    pub fn heading_mode(mut self, mode: HeadingMode) -> Self {
        self.heading_mode = mode;
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

        // Blocks come from the caller (IncrementalParser / pre-parsed) when
        // available; otherwise parse `text` here. The streaming path no longer
        // bypasses parsing — formatted markdown renders throughout streaming,
        // and `streaming` only adds the trailing cursor to the last block.
        let blocks_owned: Vec<Block> = match self.blocks {
            Some(b) => (*b).clone(),
            None => ast::parse(&self.text),
        };
        // Root carries `text_sm` so the document is self-contained: every
        // block that does not override the size itself (paragraph, list item
        // bodies) inherits it, rather than depending on the caller's ancestor.
        // The renderer is mounted inside full-width message blocks, so its root
        // must participate in the same shrink-safe width chain. Otherwise GPUI
        // may measure the content at min-content width and wrap ordinary text
        // one glyph per line. Code, diff, and table blocks still own their
        // horizontal overflow locally.
        let mut col = v_flex()
            .id(id)
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .gap_2()
            .text_sm();
        if self.scrollable {
            col = col.h_full().overflow_y_scroll();
        }
        let streaming = self.streaming;
        let block_count = blocks_owned.len();
        for (i, block) in blocks_owned.into_iter().enumerate() {
            let is_last = streaming && i == block_count.saturating_sub(1);
            col = col.child(render_block(block, &styles, self.heading_mode, i, is_last));
        }
        col.into_any_element()
    }
}

fn render_block(
    block: Block,
    styles: &MdStyles,
    mode: HeadingMode,
    idx: usize,
    streaming_tail: bool,
) -> AnyElement {
    match block {
        Block::Paragraph(runs) => paragraph(&runs, styles, streaming_tail),
        Block::Heading { runs, depth } => heading(&runs, mode, depth, styles),
        Block::Code { lang, value } => {
            code_block(&value, lang.as_deref(), styles, idx, streaming_tail)
        }
        Block::Diff { value } => diff_block(&value, styles, idx),
        Block::Conflict { value } => conflict_block(&value, styles, idx),
        Block::Blockquote(inner) => blockquote(inner, styles, mode),
        Block::List { ordered, items } => list_block(ordered, items, styles, mode),
        Block::Table { rows, align } => table_block(rows, align, styles, idx),
        Block::ThematicBreak => div()
            .w_full()
            .h(px(1.))
            .bg(styles.border)
            .into_any_element(),
    }
}

/// Map `InlineRuns::code_ranges` onto `CodeSpan` overlays carrying the
/// caller-customized wash + radius. Shared by every inline mount (paragraph,
/// heading, table cell) so the inline-code pill is consistent across them.
fn code_spans(runs: &InlineRuns, styles: &MdStyles) -> Vec<CodeSpan> {
    runs.code_ranges
        .iter()
        .map(|range| CodeSpan {
            range: range.clone(),
            bg: styles.inline_code_bg,
            radius: styles.inline_code_radius,
        })
        .collect()
}

/// Render a paragraph. When `streaming_tail` is true (the last block of a
/// mid-stream document) the cursor `▌` is appended to the paragraph text so
/// it lands at the end of the visible content. The cursor is plain text rather
/// than a separate element so it wraps with the paragraph flow.
fn paragraph(runs: &InlineRuns, styles: &MdStyles, streaming_tail: bool) -> AnyElement {
    let text = if streaming_tail {
        format!("{}▌", runs.text)
    } else {
        runs.text.clone()
    };
    div()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .text_sm()
        .child(
            RichText::anonymous(text)
                .highlights(runs.highlights.clone())
                .code_spans(code_spans(runs, styles)),
        )
        .into_any_element()
}

/// How a heading maps its depth to a renderable style. `Scaled` (default)
/// grows the font with depth; `Uniform` holds every heading at body size and
/// discriminates levels by weight + decoration, for dense mounts like the
/// message list where enlarged heading text would drown the body.
#[derive(Clone, Copy, Default)]
pub enum HeadingMode {
    #[default]
    Scaled,
    Uniform,
}

/// Per-depth heading style, mode-derived. Pure data the renderer applies
/// uniformly — the renderer holds no per-depth branching, so a new mode is a
/// new `spec` function rather than a parallel render path.
#[derive(Clone, Copy)]
struct HeadingSpec {
    weight: FontWeight,
    italic: bool,
    underline: bool,
    space_after: bool,
    size: HeadingSize,
}

#[derive(Clone, Copy)]
enum HeadingSize {
    Base,
    Sm,
}

impl HeadingSize {
    fn apply<S: Styled>(self, s: S) -> S {
        match self {
            Self::Base => s.text_base(),
            Self::Sm => s.text_sm(),
        }
    }
}

impl HeadingMode {
    fn spec(self, depth: u8) -> HeadingSpec {
        match self {
            Self::Scaled => scaled_heading(depth),
            Self::Uniform => uniform_heading(depth),
        }
    }
}

/// `Scaled`: H1/H2 stay at base size (16px) and discriminate by weight — H1
/// gets `Black` (900), H2 gets `Bold` (700). H3 is base + bold; H4+ collapse to
/// small (14px) + bold. The six-level ladder compresses to three distinguishable
/// levels without any line growing taller than the body, matching the app-wide
/// 3-font-size discipline.
fn scaled_heading(depth: u8) -> HeadingSpec {
    let (weight, size) = match depth {
        1 => (FontWeight::BLACK, HeadingSize::Base),
        2 => (FontWeight::BOLD, HeadingSize::Base),
        3 => (FontWeight::BOLD, HeadingSize::Base),
        _ => (FontWeight::BOLD, HeadingSize::Sm),
    };
    HeadingSpec {
        weight,
        italic: false,
        underline: false,
        space_after: false,
        size,
    }
}

/// `Uniform`: every heading stays at body size. Weight splits H1/H2 (black,
/// 900) from H3+ (bold, 700); italic + underline mark H1 alone; space-after
/// separates the three supported levels from the deeper ones that collapse to
/// plain bold. So the six-level ladder compresses to three distinguishable
/// levels without any line growing taller than the body.
fn uniform_heading(depth: u8) -> HeadingSpec {
    HeadingSpec {
        weight: if depth <= 2 {
            FontWeight::BLACK
        } else {
            FontWeight::BOLD
        },
        italic: depth == 1,
        underline: depth == 1,
        space_after: depth <= 3,
        size: HeadingSize::Sm,
    }
}

impl HeadingSpec {
    fn apply<S: Styled>(self, s: S) -> S {
        let s = self.size.apply(s);
        let s = s.font_weight(self.weight);
        let s = if self.italic { s.italic() } else { s };
        let s = if self.underline { s.underline() } else { s };
        if self.space_after { s.mb_2() } else { s }
    }
}

/// Heading: the depth-to-style mapping lives in `HeadingMode::spec` (pure data
/// the renderer applies), so this function is mode-agnostic. The base color is
/// inherited from the parent div — same contract as `paragraph`, so
/// streaming→finalized never recolors.
fn heading(runs: &InlineRuns, mode: HeadingMode, depth: u8, styles: &MdStyles) -> AnyElement {
    mode.spec(depth)
        .apply(div().w_full().min_w_0().overflow_hidden())
        .child(
            RichText::anonymous(runs.text.clone())
                .highlights(runs.highlights.clone())
                .code_spans(code_spans(runs, styles)),
        )
        .into_any_element()
}

/// Code block: line-number gutter (fixed during horizontal scroll) + a
/// horizontally-scrollable, non-wrapping code run with tree-sitter syntax
/// highlighting via `SyntaxHighlighter`. The run is a selectable `RichText`
/// (drag-select + Cmd/Ctrl+C copy); a hover-revealed button copies the whole
/// block for one-click capture.
fn code_block(
    value: &str,
    lang: Option<&str>,
    styles: &MdStyles,
    idx: usize,
    transient: bool,
) -> AnyElement {
    // Fenced-code values carry a trailing `\n` (the closing fence sits on its
    // own line); strip it so the gutter count and the painted run agree —
    // `split('\n')` on `"a\n"` would otherwise count a phantom empty line.
    let value = value.trim_end_matches('\n');
    let highlights = code_highlights(value, lang, styles, transient);
    let line_count = value.split('\n').count().max(1);
    let gutter: String = (1..=line_count).map(|n| format!("{n:>3}\n")).collect();
    let gutter = gutter.trim_end_matches('\n');
    let group = format!("code-{idx}");

    h_flex()
        .group(group.clone())
        .w_full()
        .min_w_0()
        .relative()
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
                    RichText::new(("code-run", idx), SharedString::from(value.to_string()))
                        .highlights(highlights)
                        .selectable(styles.selection_bg),
                ),
        )
        .child(
            div()
                .absolute()
                .top_1()
                .right_1()
                .opacity(0.)
                .group_hover(group, |s| s.opacity(1.))
                .child(copy_button(idx, value.to_string())),
        )
        .into_any_element()
}

/// Copy-the-whole-block button: writes `text` to the clipboard on click.
/// Revealed only while the enclosing `.group` is hovered.
fn copy_button(idx: usize, text: String) -> Button {
    Button::new(("code-copy", idx))
        .ghost()
        .xsmall()
        .icon(IconName::Copy)
        .on_click(move |_, _, cx: &mut App| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        })
}

/// Syntax highlighting for a code run, memoized by `(lang, content, theme)`.
/// Frozen code blocks have stable content, so after the first frame every
/// subsequent render of a completed block is a zero-work cache hit — the
/// message list re-renders every dirty frame, so this cache is what keeps a
/// long conversation from re-highlighting every block on each streaming
/// delta. Lives on the render thread — `SyntaxHighlighter` owns a thread-local
/// `Parser` and is not `Send`.
///
/// `transient` marks a still-growing tail block: its content will change on
/// the next delta, so the result is computed fresh but not written to the L2
/// cache, avoiding cache pollution with throwaway highlight data.
fn code_highlights(
    value: &str,
    lang: Option<&str>,
    styles: &MdStyles,
    transient: bool,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let lang = lang.unwrap_or("text").to_string();
    let theme_ptr = Arc::as_ptr(&styles.highlight_theme) as usize;
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    let content_hash = hasher.finish();
    let key = (lang.clone(), content_hash, theme_ptr);
    if let Some((hit, _)) = CODE_HL_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let rope = Rope::from_str(value);
    let mut hl = SyntaxHighlighter::new(&lang);
    let _ = hl.update(None, &rope, None);
    let result = hl
        .styles(&(0..value.len()), styles.highlight_theme.as_ref())
        .clone();
    if !transient {
        CODE_HL_CACHE.with(|c| c.borrow_mut().insert(key, (result.clone(), false)));
    }
    result
}

/// Unified-diff block. Each line carries its own wash + a 2px left bar in the
/// accent color; `+`/`-` prefixes are kept (TUI convention) and re-tinted to
/// the accent. No line-number gutter — hunk `@@ -a,b +c,d @@` makes per-line
/// numbers misleading. Horizontal scroll keeps long added/removed runs aligned.
fn diff_block(value: &str, styles: &MdStyles, idx: usize) -> AnyElement {
    let mut inner = v_flex().min_w_0();
    for (i, line) in value.lines().enumerate() {
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
                // Per-line selectable run: selection does not cross lines
                // (acceptable for diff — each hunk line is read in isolation).
                // The id is unique within the block via the ancestor
                // `("diff-scroll", idx)` id in the element-id stack.
                .child(
                    RichText::new(("line", i), SharedString::from(line.to_string()))
                        .selectable(styles.selection_bg),
                ),
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

/// Per-line role within a git merge-conflict blob. The four marker lines
/// become section headers; content lines carry the wash + bar of the section
/// they belong to (ours / base / theirs), and `Context` covers any lines
/// outside the marker region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConflictLineKind {
    Context,
    Ours,
    Base,
    Theirs,
    MarkerOurs,
    MarkerBase,
    MarkerSep,
    MarkerTheirsEnd,
}

/// State carried between lines while classifying a conflict blob: which side
/// content lines belong to until the next marker flips it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConflictSection {
    Context,
    Ours,
    Base,
    Theirs,
}

/// Split a conflict blob into per-line roles. `<<<<<<<`/`=======`/`|||||||`/
/// `>>>>>>>` are recognized at line start; the `=======` separator is matched
/// exactly (git emits precisely seven `=` with nothing else on the line) so a
/// content line beginning with `=` is not mistaken for it.
fn parse_conflict(value: &str) -> Vec<(&str, ConflictLineKind)> {
    let mut state = ConflictSection::Context;
    let mut out = Vec::new();
    for line in value.lines() {
        let kind = if line.starts_with("<<<<<<<") {
            state = ConflictSection::Ours;
            ConflictLineKind::MarkerOurs
        } else if line.starts_with("|||||||") {
            state = ConflictSection::Base;
            ConflictLineKind::MarkerBase
        } else if line == "=======" {
            state = ConflictSection::Theirs;
            ConflictLineKind::MarkerSep
        } else if line.starts_with(">>>>>>>") {
            state = ConflictSection::Context;
            ConflictLineKind::MarkerTheirsEnd
        } else {
            match state {
                ConflictSection::Context => ConflictLineKind::Context,
                ConflictSection::Ours => ConflictLineKind::Ours,
                ConflictSection::Base => ConflictLineKind::Base,
                ConflictSection::Theirs => ConflictLineKind::Theirs,
            }
        };
        out.push((line, kind));
    }
    out
}

/// (left bar, background, foreground, bold) for one conflict line.
fn conflict_style(kind: ConflictLineKind, styles: &MdStyles) -> (Hsla, Hsla, Hsla, bool) {
    use ConflictLineKind::*;
    match kind {
        Context => (
            styles.transparent,
            styles.transparent,
            styles.foreground,
            false,
        ),
        Ours => (
            styles.diff_add_fg,
            styles.diff_add_bg,
            styles.foreground,
            false,
        ),
        MarkerOurs => (
            styles.diff_add_fg,
            styles.diff_add_fg.opacity(0.22),
            styles.diff_add_fg,
            true,
        ),
        Base => (styles.muted, styles.secondary, styles.foreground, false),
        MarkerBase => (styles.muted, styles.muted.opacity(0.18), styles.muted, true),
        Theirs => (
            styles.diff_del_fg,
            styles.diff_del_bg,
            styles.foreground,
            false,
        ),
        // The `=======` separator is the boundary between sides, so it stays
        // neutral rather than taking either side's color.
        MarkerSep => (styles.muted, styles.secondary, styles.muted, true),
        MarkerTheirsEnd => (
            styles.diff_del_fg,
            styles.diff_del_fg.opacity(0.22),
            styles.diff_del_fg,
            true,
        ),
    }
}

/// git merge-conflict block. Each line carries its section's wash + a 2px left
/// bar; the `<<<<<<<`/`|||||||`/`=======`/`>>>>>>>` marker lines become bold
/// section headers tinted in their side's accent (green ours, red theirs,
/// muted base/separator), so the two sides read at a glance. Horizontal scroll
/// keeps long content rows aligned; no line-number gutter — conflict markers
/// are section-based, not line-numbered.
fn conflict_block(value: &str, styles: &MdStyles, idx: usize) -> AnyElement {
    let value = value.trim_end_matches('\n');
    let mut inner = v_flex().min_w_0();
    for (i, (line, kind)) in parse_conflict(value).into_iter().enumerate() {
        let (bar, bg, fg, bold) = conflict_style(kind, styles);
        let mut row = div()
            .border_l_2()
            .border_color(bar)
            .bg(bg)
            .px_3()
            .py(px(1.))
            .text_xs()
            .text_color(fg)
            .whitespace_nowrap()
            .child(
                RichText::new(("line", i), SharedString::from(line.to_string()))
                    .selectable(styles.selection_bg),
            );
        if bold {
            row = row.font_weight(FontWeight::BOLD);
        }
        inner = inner.child(row);
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
                .id(("conflict-scroll", idx))
                .w_full()
                .min_w_0()
                .overflow_x_scroll()
                .child(inner),
        )
        .into_any_element()
}

fn blockquote(inner: Vec<Block>, styles: &MdStyles, mode: HeadingMode) -> AnyElement {
    let mut col = v_flex()
        .w_full()
        .min_w_0()
        .border_l_2()
        .border_color(styles.border)
        .pl_3()
        .gap_2();
    for (i, block) in inner.into_iter().enumerate() {
        col = col.child(render_block(block, styles, mode, i, false));
    }
    col.into_any_element()
}

/// GFM table. Rows stretch to the scroll width so `flex_1` cells land in
/// equal-width columns and align row-to-row; a per-cell `min_w` floor keeps
/// wide tables from collapsing, overflowing horizontally into the scroll
/// viewport instead of clipping. Every cell carries right + bottom borders so
/// the grid is visible even on the transparent body rows.
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
        .overflow_x_scroll();
    for (r, row) in rows.into_iter().enumerate() {
        let is_header = r == 0;
        let mut row_flex = h_flex().min_w_0().w_full();
        for (c, cell) in row.into_iter().enumerate() {
            let mut cell_div = div()
                .flex_1()
                .min_w(px(140.))
                .px_3()
                .py_2()
                .text_xs()
                .border_r_1()
                .border_b_1()
                .border_color(styles.border)
                .child(
                    RichText::anonymous(cell.text.clone())
                        .highlights(cell.highlights.clone())
                        .code_spans(code_spans(&cell, styles)),
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

fn list_block(
    ordered: bool,
    items: Vec<ListItem>,
    styles: &MdStyles,
    mode: HeadingMode,
) -> AnyElement {
    let mut col = v_flex().w_full().min_w_0().gap_1();
    for (i, item) in items.into_iter().enumerate() {
        let mut item_col = v_flex().flex_1().min_w_0().gap_1();
        for (j, b) in item.blocks.into_iter().enumerate() {
            item_col = item_col.child(render_block(b, styles, mode, j, false));
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
    fn uniform_heading_compresses_six_levels_to_three() {
        // H1: black weight + italic + underline + space-after, body size.
        let h1 = HeadingMode::Uniform.spec(1);
        assert_eq!(h1.weight, FontWeight::BLACK);
        assert!(h1.italic && h1.underline && h1.space_after);
        assert!(matches!(h1.size, HeadingSize::Sm));

        // H2: black weight, no italic/underline, space-after.
        let h2 = HeadingMode::Uniform.spec(2);
        assert_eq!(h2.weight, FontWeight::BLACK);
        assert!(!h2.italic && !h2.underline && h2.space_after);

        // H3: bold weight, space-after.
        let h3 = HeadingMode::Uniform.spec(3);
        assert_eq!(h3.weight, FontWeight::BOLD);
        assert!(!h3.italic && !h3.underline && h3.space_after);

        // H4–H6 collapse to plain bold, no space-after — indistinguishable.
        for depth in 4..=6 {
            let h = HeadingMode::Uniform.spec(depth);
            assert_eq!(h.weight, FontWeight::BOLD, "depth {depth}");
            assert!(!h.italic && !h.underline && !h.space_after, "depth {depth}");
            assert!(matches!(h.size, HeadingSize::Sm), "depth {depth}");
        }
    }

    #[test]
    fn scaled_heading_converges_to_three_sizes() {
        // D3 convergence: H1=base+Black, H2/H3=base+Bold, H4+=small+Bold.
        let h1 = HeadingMode::Scaled.spec(1);
        assert!(matches!(h1.size, HeadingSize::Base));
        assert_eq!(h1.weight, FontWeight::BLACK);

        let h2 = HeadingMode::Scaled.spec(2);
        assert!(matches!(h2.size, HeadingSize::Base));
        assert_eq!(h2.weight, FontWeight::BOLD);

        let h3 = HeadingMode::Scaled.spec(3);
        assert!(matches!(h3.size, HeadingSize::Base));
        assert_eq!(h3.weight, FontWeight::BOLD);

        for depth in 4..=6 {
            let h = HeadingMode::Scaled.spec(depth);
            assert!(matches!(h.size, HeadingSize::Sm), "depth {depth}");
            assert_eq!(h.weight, FontWeight::BOLD, "depth {depth}");
            assert!(!h.space_after, "depth {depth}");
        }
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

    #[test]
    fn conflict_parser_assigns_sections() {
        let blob = "\
<<<<<<< HEAD
fn a() {}
=======
fn a() -> u32 { 0 }
>>>>>>> main";
        let parsed = parse_conflict(blob);
        use ConflictLineKind::*;
        let kinds: Vec<_> = parsed.iter().map(|(_, k)| *k).collect();
        assert_eq!(
            kinds,
            vec![MarkerOurs, Ours, MarkerSep, Theirs, MarkerTheirsEnd]
        );
    }

    #[test]
    fn conflict_parser_handles_diff3_base_section() {
        let blob = "\
<<<<<<< HEAD
ours
||||||| base
base
=======
theirs
>>>>>>> main";
        let parsed = parse_conflict(blob);
        use ConflictLineKind::*;
        let kinds: Vec<_> = parsed.iter().map(|(_, k)| *k).collect();
        assert_eq!(
            kinds,
            vec![
                MarkerOurs,
                Ours,
                MarkerBase,
                Base,
                MarkerSep,
                Theirs,
                MarkerTheirsEnd
            ]
        );
    }

    #[test]
    fn conflict_separator_is_exact_seven_equals() {
        // A content line beginning with `=` (even seven of them, if followed
        // by more) must not be mistaken for the `=======` separator.
        let blob = "\
<<<<<<< HEAD
=======x not a separator
=======
real theirs
>>>>>>> main";
        let parsed = parse_conflict(blob);
        use ConflictLineKind::*;
        // The `=======x` line is ours content (only an exact `=======` flips
        // to theirs), so the second `=======` is the real separator.
        assert_eq!(parsed[1].1, Ours);
        assert_eq!(parsed[2].1, MarkerSep);
        assert_eq!(parsed[3].1, Theirs);
    }

    #[test]
    fn conflict_context_lines_outside_markers() {
        // Lines before the opening and after the closing marker are context.
        let blob = "\
prefix line
<<<<<<< HEAD
ours
>>>>>>> main
suffix line";
        let parsed = parse_conflict(blob);
        assert_eq!(parsed[0].1, ConflictLineKind::Context);
        assert_eq!(parsed[4].1, ConflictLineKind::Context);
    }

    #[test]
    fn conflict_style_marks_separator_neutral() {
        let s = styles();
        // The `=======` separator must take the muted palette, not either
        // side's accent — it is the boundary, not part of a side.
        let (bar, _, fg, _) = conflict_style(ConflictLineKind::MarkerSep, &s);
        assert_eq!(bar, s.muted);
        assert_eq!(fg, s.muted);
    }
}
