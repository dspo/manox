//! Markdown renderer — a stateful `Entity` owning the source, the incremental
//! parser, and a document-level selection.
//!
//! Replaces the stateless `gpui_component::text::TextView::markdown` with a
//! parse-once cache and cross-block text selection: one `DocSelection` spans
//! every text-bearing block of one document, so a drag that crosses paragraph /
//! code / list boundaries extends one continuous selection, and Cmd/Ctrl+C
//! copies the selected text.
//!
//! Architecture: `Markdown` is an `Entity` (`Render`). Its `render` builds a
//! vertical flex root that is focusable (so its key listener fires on Cmd/Ctrl+C
//! after a click-to-select) and mounts a zero-size sentinel element as the first
//! child — the sentinel clears the per-frame block registry at paint start, then
//! each block's `RichText` re-registers its geometry during paint. The root's
//! mouse listeners hit-test against that registry to drive the shared
//! `DocSelection`; the key listener copies it. `RichText` composes `StyledText`
//! for shaping/glyph-painting and overlays rounded inline-code washes + the
//! document-selection slice for the block. The base font/color is inherited from
//! `window.text_style()` (set by the parent `div`'s `.text_sm()`/`.text_color()`/…)
//! at layout time, so the renderer never constructs a `TextStyle`.

pub mod ast;
pub mod incremental;
pub mod rich_text;
pub mod selection;
pub mod terminal_panel;
pub mod theme;

pub use terminal_panel::{PanelKind, TerminalPanel};

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClipboardItem, Element, ElementId, FocusHandle, FontWeight, GlobalElementId,
    HighlightStyle, Hsla, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Render, SharedString, Style, Window, div, px,
};
use gpui_component::highlighter::SyntaxHighlighter;
use gpui_component::{
    IconName, Sizable, Theme,
    button::{Button, ButtonVariants},
    h_flex, v_flex,
};
use ropey::Rope;

use crate::markdown::ast::{Block, InlineRuns, LinkKind, LinkSpan, ListItem, TableAlign};
use crate::markdown::incremental::IncrementalParser;
use crate::markdown::rich_text::{CodeSpan, RichText};
use crate::markdown::selection::DocSelection;
use crate::markdown::theme::MdStyles;

// Per-(language, content, highlight theme) highlight cache. The message list
// re-renders every dirty frame (a streaming delta dirties the workspace, and
// every frozen block re-paints with it); without this cache each frame would
// rebuild a `SyntaxHighlighter`, re-parse, and re-query styles for *every*
// code block — including completed ones whose content never changes. The
// highlight theme pointer is part of the key so a theme swap invalidates the
// cache automatically; manox has no runtime theme switch today, so in practice
// the key reduces to `(lang, content_hash)`.
//
// `transient` marks an entry written while rendering a still-streaming tail
// block — its content is incomplete and will change on the next delta, so the
// L2 cache write is skipped to avoid polluting the cache with throwaway
// highlight data.
type HighlightCache = HashMap<(String, u64, usize), (Vec<(Range<usize>, HighlightStyle)>, bool)>;

thread_local! {
    static CODE_HL_CACHE: RefCell<HighlightCache> = RefCell::new(HashMap::new());
}

/// Stateful markdown document: owns the source, the incremental parser, and the
/// document-level selection. Parse-once: the `IncrementalParser` freezes the
/// completed prefix so a streaming append only re-parses the growing tail, and
/// `parsed()` hands the cached block list to the renderer.
pub struct Markdown {
    id: ElementId,
    source: SharedString,
    parser: IncrementalParser,
    styles: Option<MdStyles>,
    scrollable: bool,
    streaming: bool,
    heading_mode: HeadingMode,
    selection: DocSelection,
    /// Lazily created on first render so `new` needs no `cx` (callers construct
    /// `Markdown` inside `cx.new` and may not have a handle handy).
    focus: Option<FocusHandle>,
    /// Optional callback invoked on Cmd+click of a link span. The opener
    /// receives the link URL and kind; the caller decides what action to
    /// take (open in browser, VS Code, etc.).
    link_opener: Option<Arc<dyn Fn(String, LinkKind) + Send + Sync>>,
}

impl Markdown {
    /// Construct from a finalized (or initial) source. Parses synchronously into
    /// the incremental parser's frozen prefix. Streaming bodies call `append` /
    /// `replace` afterwards to grow the source.
    pub fn new(id: impl Into<ElementId>, source: impl Into<SharedString>) -> Self {
        let source = source.into();
        let mut parser = IncrementalParser::new();
        parser.update(&source);
        Self {
            id: id.into(),
            source,
            parser,
            styles: None,
            scrollable: false,
            streaming: false,
            heading_mode: HeadingMode::default(),
            selection: DocSelection::new(),
            focus: None,
            link_opener: None,
        }
    }

    /// Bridge the workspace theme (colors + syntax highlight palette) into the
    /// renderer's style table. Without this the renderer paints nothing.
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

    /// Override the inline-code pill wash + corner radius. Without this the
    /// renderer falls back to `theme.secondary` / `theme.radius`.
    pub fn inline_code(mut self, bg: Hsla, radius: Pixels) -> Self {
        if let Some(styles) = &mut self.styles {
            styles.inline_code_bg = bg;
            styles.inline_code_radius = radius;
        }
        self
    }

    /// Mark the document as mid-stream: a trailing cursor `▌` is appended to the
    /// last paragraph's text so it lands at the end of the visible content. The
    /// full markdown layout is rendered throughout streaming (from the parser's
    /// blocks); `streaming` only adds the trailing cursor.
    pub fn streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }

    /// How headings map depth to style. `Scaled` (default) grows the font with
    /// depth; `Uniform` holds every heading at body size and discriminates levels
    /// by weight + decoration, for dense mounts like the message list where
    /// enlarged heading text would drown the body.
    pub fn heading_mode(mut self, mode: HeadingMode) -> Self {
        self.heading_mode = mode;
        self
    }

    /// Set a callback invoked on Cmd+click of a detected link span. Pass `None`
    /// to disable link opening (links are still underlined).
    pub fn on_open_link(
        mut self,
        opener: Option<Arc<dyn Fn(String, LinkKind) + Send + Sync>>,
    ) -> Self {
        self.link_opener = opener;
        self
    }

    /// Append `delta` to the source and re-parse incrementally. Only the growing
    /// tail is re-parsed; the frozen prefix is reused.
    pub fn append(&mut self, delta: &str, cx: &mut gpui::Context<Self>) {
        let next: SharedString = format!("{}{}", self.source, delta).into();
        self.source = next;
        self.parser.update(&self.source);
        cx.notify();
    }

    /// Replace the whole source. A non-append-only change falls back to a full
    /// parse inside the parser; either way `parsed()` reflects the new text.
    pub fn replace(&mut self, source: impl Into<SharedString>, cx: &mut gpui::Context<Self>) {
        self.source = source.into();
        self.parser.update(&self.source);
        cx.notify();
    }

    /// Reset to a fresh source, dropping the frozen prefix so the parser starts
    /// over. Use when the document identity changes (not just appends).
    pub fn reset(&mut self, source: impl Into<SharedString>, cx: &mut gpui::Context<Self>) {
        self.source = source.into();
        self.parser = IncrementalParser::new();
        self.parser.update(&self.source);
        cx.notify();
    }

    /// The full document text at the last update.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether a background parse is in flight. The incremental parser runs
    /// synchronously on the gpui thread (append-only tail parse is sub-frame),
    /// so this is always false today; the seam is kept for a future async parse.
    pub fn is_parsing(&self) -> bool {
        false
    }

    /// The cached parsed blocks (frozen prefix + re-parsed tail).
    pub fn parsed(&self) -> Arc<Vec<Block>> {
        self.parser.blocks()
    }

    /// Drop the streaming flag and run the parser's final full parse so the
    /// frozen prefix + tail match a one-shot full parse exactly (the tail may
    /// have been held back by the `\n\n` boundary guard mid-stream). Idempotent.
    pub fn finalize_streaming(&mut self, cx: &mut gpui::Context<Self>) {
        self.streaming = false;
        self.parser.finalize();
        cx.notify();
    }

    /// Run the parser's final full parse without touching the streaming flag.
    /// Used when a finalized (non-streaming) body is loaded from history and
    /// needs its blocks populated to match a one-shot parse.
    pub fn finalize(&mut self, cx: &mut gpui::Context<Self>) {
        self.parser.finalize();
        cx.notify();
    }
}

impl Render for Markdown {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let Some(styles) = self.styles.clone() else {
            return div().id(self.id.clone()).into_any_element();
        };

        // Lazily create the focus handle on first render.
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        let selection = self.selection.clone();
        let streaming = self.streaming;
        let heading_mode = self.heading_mode;

        let blocks = self.parser.blocks();
        let block_count = blocks.len();
        let blocks_owned: Vec<Block> = (*blocks).clone();

        // Root carries `text_sm` so the document is self-contained: every block
        // that does not override the size itself (paragraph, list item bodies)
        // inherits it. The renderer is mounted inside full-width message blocks,
        // so its root must participate in the same shrink-safe width chain.
        let mut col = v_flex()
            .id(self.id.clone())
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .gap_2()
            .text_sm()
            // I-beam over the document body: a clickable, selectable text
            // surface signals itself to the pointer.
            .cursor_text()
            .track_focus(&focus)
            // The sentinel must be the first child so its paint (clearing the
            // per-frame registry) runs before any block registers.
            .child(Sentinel {
                selection: selection.clone(),
            });

        if self.scrollable {
            col = col.h_full().overflow_y_scroll();
        }

        let mut cursor = 0usize;
        for (i, block) in blocks_owned.into_iter().enumerate() {
            let is_last = streaming && i == block_count.saturating_sub(1);
            col = col.child(render_block(
                block,
                &styles,
                heading_mode,
                i,
                is_last,
                &mut cursor,
                &selection,
            ));
        }

        col.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                if let Some(ix) = this.selection.hit(e.position) {
                    // Cmd+click on a link: open it, skip text selection.
                    if e.modifiers.platform
                        && let Some(link) = this.selection.link_at(ix)
                    {
                        if let Some(ref opener) = this.link_opener {
                            opener(link.url.clone(), link.kind);
                        }
                        cx.notify();
                        return;
                    }
                    match e.click_count {
                        2 => this.selection.select_word(ix),
                        n if n >= 3 => this.selection.select_line(ix),
                        _ => this.selection.begin(ix),
                    }
                    window.focus(this.focus.as_ref().expect("focus init in render"), cx);
                }
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(move |this, e: &MouseMoveEvent, _window, cx| {
            if this.selection.is_dragging()
                && let Some(ix) = this.selection.hit(e.position)
            {
                this.selection.extend(ix);
                cx.notify();
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, _e: &MouseUpEvent, _window, cx| {
                this.selection.end();
                cx.notify();
            }),
        )
        .on_key_down(
            cx.listener(move |this, e: &gpui::KeyDownEvent, _window, cx| {
                let k = &e.keystroke;
                if k.modifiers.secondary() && k.key == "c" {
                    if this.selection.range().is_some() {
                        this.selection.copy_to_clipboard(cx);
                        cx.stop_propagation();
                    }
                } else if k.key == "escape" {
                    // Escape clears a stale selection without copying.
                    this.selection.end();
                    cx.notify();
                }
            }),
        )
        .into_any_element()
    }
}

/// Zero-size paint-only element that clears the document-selection registry at
/// the start of each frame. Mounted as the first child of the markdown root so
/// its paint precedes every block's register call; the registry the root's
/// mouse/keyboard listeners read is therefore always the freshly rebuilt one
/// from the most recent paint.
struct Sentinel {
    selection: DocSelection,
}

impl IntoElement for Sentinel {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Sentinel {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (window.request_layout(Style::default(), None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
        self.selection.clear_registry();
    }
}

fn render_block(
    block: Block,
    styles: &MdStyles,
    mode: HeadingMode,
    idx: usize,
    streaming_tail: bool,
    cursor: &mut usize,
    selection: &DocSelection,
) -> AnyElement {
    match block {
        Block::Paragraph(runs) => paragraph(&runs, styles, streaming_tail, cursor, selection),
        Block::Heading { runs, depth } => heading(&runs, mode, depth, styles, cursor, selection),
        Block::Code { lang, value } => code_block(
            &value,
            lang.as_deref(),
            styles,
            idx,
            streaming_tail,
            cursor,
            selection,
        ),
        Block::Diff { value } => diff_block(&value, styles, idx, cursor, selection),
        Block::Conflict { value } => conflict_block(&value, styles, idx, cursor, selection),
        Block::Blockquote(inner) => blockquote(inner, styles, mode, idx, cursor, selection),
        Block::List { ordered, items } => {
            list_block(ordered, items, styles, mode, idx, cursor, selection)
        }
        Block::Table { rows, align } => table_block(rows, align, styles, idx, cursor, selection),
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

/// Map `InlineRuns::link_spans` through unchanged — the spans are
/// already fully-formed. Shared by every inline mount so Cmd+click
/// resolution is consistent across paragraph, heading, and table cells.
fn link_spans(runs: &InlineRuns) -> Vec<LinkSpan> {
    runs.link_spans.clone()
}

/// Render a paragraph. When `streaming_tail` is true (the last block of a
/// mid-stream document) the cursor `▌` is appended to the paragraph text so it
/// lands at the end of the visible content. The cursor is plain text rather
/// than a separate element so it wraps with the paragraph flow; its bytes are
/// part of the block's selectable range.
fn paragraph(
    runs: &InlineRuns,
    styles: &MdStyles,
    streaming_tail: bool,
    cursor: &mut usize,
    selection: &DocSelection,
) -> AnyElement {
    let text = if streaming_tail {
        format!("{}▌", runs.text)
    } else {
        runs.text.clone()
    };
    let doc_start = *cursor;
    *cursor += text.len();
    div()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .text_sm()
        .child(
            RichText::new(text, doc_start, selection.clone())
                .highlights(runs.highlights.clone())
                .code_spans(code_spans(runs, styles))
                .link_spans(link_spans(runs))
                .link_color(styles.link_color)
                .selection_bg(styles.selection_bg),
        )
        .into_any_element()
}
/// How a heading maps its depth to a renderable style. `Scaled` (default) grows
/// the font with depth; `Uniform` holds every heading at body size and
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
fn heading(
    runs: &InlineRuns,
    mode: HeadingMode,
    depth: u8,
    styles: &MdStyles,
    cursor: &mut usize,
    selection: &DocSelection,
) -> AnyElement {
    let doc_start = *cursor;
    *cursor += runs.text.len();
    mode.spec(depth)
        .apply(div().w_full().min_w_0().overflow_hidden())
        .child(
            RichText::new(runs.text.clone(), doc_start, selection.clone())
                .highlights(runs.highlights.clone())
                .code_spans(code_spans(runs, styles))
                .link_spans(link_spans(runs))
                .link_color(styles.link_color)
                .selection_bg(styles.selection_bg),
        )
        .into_any_element()
}

/// Code block: line-number gutter (fixed during horizontal scroll) + a
/// horizontally-scrollable, non-wrapping code run with tree-sitter syntax
/// highlighting via `SyntaxHighlighter`. The run participates in the document
/// selection; a hover-revealed button copies the whole block for one-click
/// capture.
fn code_block(
    value: &str,
    lang: Option<&str>,
    styles: &MdStyles,
    idx: usize,
    transient: bool,
    cursor: &mut usize,
    selection: &DocSelection,
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

    let doc_start = *cursor;
    *cursor += value.len();

    h_flex()
        .group(group.clone())
        .w_full()
        .min_w_0()
        .relative()
        .rounded_md()
        // Transparent surface + hairline border instead of a solid `secondary`
        // block: keeps the code panel readable on the message surface without
        // stacking gray rectangles down the list. Syntax highlighting and the
        // gutter carry the structure; only hover/selection add local fill.
        .border_1()
        .border_color(styles.border)
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
                    RichText::new(
                        SharedString::from(value.to_string()),
                        doc_start,
                        selection.clone(),
                    )
                    .highlights(highlights)
                    .selection_bg(styles.selection_bg),
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
fn diff_block(
    value: &str,
    styles: &MdStyles,
    idx: usize,
    cursor: &mut usize,
    selection: &DocSelection,
) -> AnyElement {
    let mut inner = v_flex().min_w_0();
    for (i, line) in value.lines().enumerate() {
        let (bg, bar, fg) = classify_diff_line(line, styles);
        let doc_start = *cursor;
        *cursor += line.len();
        // First line of the block is a block boundary (`"\n\n"`); subsequent
        // lines are continuation lines within the same block (`"\n"`), so a
        // multi-line diff copy joins with single newlines.
        let join_before = if i == 0 { "\n\n" } else { "\n" };
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
                .child(
                    RichText::new(
                        SharedString::from(line.to_string()),
                        doc_start,
                        selection.clone(),
                    )
                    .join_before(join_before)
                    .selection_bg(styles.selection_bg),
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
fn conflict_block(
    value: &str,
    styles: &MdStyles,
    idx: usize,
    cursor: &mut usize,
    selection: &DocSelection,
) -> AnyElement {
    let value = value.trim_end_matches('\n');
    let mut inner = v_flex().min_w_0();
    for (i, (line, kind)) in parse_conflict(value).into_iter().enumerate() {
        let (bar, bg, fg, bold) = conflict_style(kind, styles);
        let doc_start = *cursor;
        *cursor += line.len();
        let join_before = if i == 0 { "\n\n" } else { "\n" };
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
                RichText::new(
                    SharedString::from(line.to_string()),
                    doc_start,
                    selection.clone(),
                )
                .join_before(join_before)
                .selection_bg(styles.selection_bg),
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

fn blockquote(
    inner: Vec<Block>,
    styles: &MdStyles,
    mode: HeadingMode,
    idx: usize,
    cursor: &mut usize,
    selection: &DocSelection,
) -> AnyElement {
    let mut col = v_flex()
        .id(("md-bq", idx))
        .w_full()
        .min_w_0()
        .border_l_2()
        .border_color(styles.border)
        .pl_3()
        .gap_2();
    for (i, block) in inner.into_iter().enumerate() {
        col = col.child(render_block(
            block, styles, mode, i, false, cursor, selection,
        ));
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
    cursor: &mut usize,
    selection: &DocSelection,
) -> AnyElement {
    let mut scroll = v_flex()
        .id(("table", idx))
        .w_full()
        .min_w_0()
        .overflow_x_scroll();
    for (r, row) in rows.into_iter().enumerate() {
        let is_header = r == 0;
        // `items_stretch` is load-bearing: `h_flex()` defaults to cross-axis
        // centering, so each cell would take its own content height and draw
        // `border_b` at a different y when a cell wraps to multiple lines —
        // producing the split "double bottom border" per row. Stretching every
        // cell to the row's max height makes the per-cell `border_b` land on
        // one shared baseline, so each row shows a single aligned border.
        let mut row_flex = h_flex()
            .id(("md-row", r))
            .min_w_0()
            .w_full()
            .items_stretch();
        for (c, cell) in row.into_iter().enumerate() {
            let doc_start = *cursor;
            *cursor += cell.text.len();
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
                    RichText::new(cell.text.clone(), doc_start, selection.clone())
                        .highlights(cell.highlights.clone())
                        .code_spans(code_spans(&cell, styles))
                        .link_spans(link_spans(&cell))
                        .link_color(styles.link_color)
                        .selection_bg(styles.selection_bg),
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
    idx: usize,
    cursor: &mut usize,
    selection: &DocSelection,
) -> AnyElement {
    let mut col = v_flex().id(("md-list", idx)).w_full().min_w_0().gap_1();
    for (i, item) in items.into_iter().enumerate() {
        let mut item_col = v_flex().flex_1().min_w_0().gap_1();
        for (j, b) in item.blocks.into_iter().enumerate() {
            item_col = item_col.child(render_block(b, styles, mode, j, false, cursor, selection));
        }
        col = col.child(
            h_flex()
                .id(("md-li", i))
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
    use gpui::TestAppContext;
    use gpui_component::Theme;

    fn styles() -> MdStyles {
        MdStyles::from_theme(&Theme::default())
    }

    #[test]
    fn parse_initial_source_freezes_blocks() {
        let md = Markdown::new("m", "hello\n\nworld");
        let blocks = md.parsed();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0], Block::Paragraph(_)));
    }

    #[test]
    fn append_grows_source_and_blocks() {
        let mut md = Markdown::new("m", "alpha");
        // Simulate an append without a cx (tests the parser wiring directly).
        let next = format!("{}{}", md.source(), "\n\nbeta");
        md.parser.update(&next);
        md.source = next.into();
        let blocks = md.parsed();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[1], Block::Paragraph(_)));
    }

    #[test]
    fn replace_swaps_source() {
        let mut md = Markdown::new("m", "old text");
        let next = "completely new text".to_string();
        md.parser.update(&next);
        md.source = next.clone().into();
        assert_eq!(md.source(), next);
        let blocks = md.parsed();
        assert_eq!(blocks.len(), 1);
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

    /// Headless visual spike: render a `Markdown` in a real (test-platform)
    /// window, park, and confirm the renderer paints without panicking and the
    /// parsed blocks survive a real layout/paint cycle. Guards the assumption
    /// that visual-test infra can drive this renderer before heavier
    /// selection/scroll tests lean on it.
    #[gpui::test]
    async fn renders_blocks_in_window(cx: &mut TestAppContext) {
        let source = "# Heading\n\nfirst paragraph\n\nsecond paragraph";
        let (view, cx) = cx.add_window_view(|_window, _cx| {
            Markdown::new("m", source)
                .theme(&Theme::default())
                .heading_mode(HeadingMode::Uniform)
        });
        cx.run_until_parked();
        let blocks = view.read_with(cx, |m, _| m.parsed());
        assert!(
            blocks.len() >= 3,
            "expected >=3 blocks after paint, got {}",
            blocks.len()
        );
    }

    /// Decide whether `.italic()` on a wrapper div reaches a probe laid out
    /// inside a nested `Entity<T>` view. The reasoning body is a persistent
    /// `Entity<Markdown>` mounted under an `.italic()` wrapper; tool-call
    /// chrome (plain divs) keeps its italic while reasoning loses it, so the
    /// suspect is the view boundary. This probe snapshots
    /// `window.text_style().font_style` during `request_layout` at the probe's
    /// depth and asserts the italic refinement crossed the boundary.
    #[gpui::test]
    async fn italic_propagates_across_entity_boundary(cx: &mut TestAppContext) {
        use gpui::{
            App, Bounds, Element, FontStyle, GlobalElementId, InspectorElementId, IntoElement,
            LayoutId, Pixels, Render, Style, Window, div,
        };
        use std::cell::Cell;
        use std::rc::Rc;

        // A leaf element whose only job is to snapshot the active text style's
        // font_style during request_layout, so the window's text-style stack is
        // observable at the probe's layout depth.
        struct Probe {
            captured: Rc<Cell<Option<FontStyle>>>,
        }
        impl IntoElement for Probe {
            type Element = Self;
            fn into_element(self) -> Self {
                self
            }
        }
        impl Element for Probe {
            type RequestLayoutState = ();
            type PrepaintState = ();
            fn id(&self) -> Option<gpui::ElementId> {
                None
            }
            fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
                None
            }
            fn request_layout(
                &mut self,
                _id: Option<&GlobalElementId>,
                _inspector_id: Option<&InspectorElementId>,
                window: &mut Window,
                cx: &mut App,
            ) -> (LayoutId, ()) {
                self.captured.set(Some(window.text_style().font_style));
                (
                    window.request_layout(Style::default(), None::<LayoutId>, cx),
                    (),
                )
            }
            fn prepaint(
                &mut self,
                _id: Option<&GlobalElementId>,
                _inspector_id: Option<&InspectorElementId>,
                _bounds: Bounds<Pixels>,
                _request_layout: &mut (),
                _window: &mut Window,
                _cx: &mut App,
            ) {
            }
            fn paint(
                &mut self,
                _id: Option<&GlobalElementId>,
                _inspector_id: Option<&InspectorElementId>,
                _bounds: Bounds<Pixels>,
                _request_layout: &mut (),
                _prepaint: &mut (),
                _window: &mut Window,
                _cx: &mut App,
            ) {
            }
        }

        // Mirrors the Markdown root col: a plain `div().text_sm()` whose only
        // child is the probe, mounted as an `Entity` so the probe's layout runs
        // across the view boundary exactly like a persistent `Entity<Markdown>`.
        struct Host {
            probe: Rc<Cell<Option<FontStyle>>>,
        }
        impl Render for Host {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                div().text_sm().child(Probe {
                    captured: self.probe.clone(),
                })
            }
        }

        // Root wraps the `Host` entity in an `.italic()` div, matching the
        // reasoning body wrapper.
        struct Root {
            host: gpui::Entity<Host>,
        }
        impl Render for Root {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                div().italic().child(self.host.clone())
            }
        }

        let probe = Rc::new(Cell::new(None::<FontStyle>));
        let probe_for_build = probe.clone();
        let (_view, cx) = cx.add_window_view(move |_window, cx| {
            let host = cx.new(|_cx| Host {
                probe: probe_for_build.clone(),
            });
            Root { host }
        });
        cx.run_until_parked();

        let captured = probe.get();
        assert_eq!(
            captured,
            Some(FontStyle::Italic),
            "italic on the wrapper div must reach a probe inside a nested Entity view"
        );
    }

    /// A real mouse drag from the first paragraph, across a code block, into the
    /// last paragraph produces one continuous document selection, and
    /// Cmd/Ctrl+C copies the spanned text. This is the runtime seat of the
    /// cross-block selection + copy guarantee: the drag hit-tests the painted
    /// per-frame registry across paragraph/code/paragraph boundaries, the key
    /// listener copies the resulting range, and the clipboard lands the joined
    /// text from more than one block.
    #[gpui::test]
    async fn drag_across_blocks_copies_joined_text(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseButton, point, px, size};
        let source = "alpha beta gamma\n\n```rust\nlet x = 1;\n```\n\ndelta epsilon zeta";
        // The code-block path reads the gpui-component global theme, so init it
        // before the window renders.
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|_window, _cx| {
            Markdown::new("m", source)
                .theme(&Theme::default())
                // `scrollable` makes the root `h_full` so it fills the window and
                // mouse events anywhere land on the column's listeners (a
                // content-height root would leave the area below the last block
                // uncovered and the drag's tail would miss).
                .scrollable(true)
        });
        cx.simulate_resize(size(px(800.), px(600.)));
        cx.run_until_parked();

        // Probe the painted registry for the extreme doc indices, then drag
        // anchor→caret between them. `hit` snap-to-boundary covers margins, so
        // the probe reliably resolves a point in the first block and one past
        // the last.
        let (anchor_pt, caret_pt) = view.read_with(cx, |m, _| {
            let sel = m.selection.clone();
            let mut first: Option<(gpui::Point<Pixels>, usize)> = None;
            let mut last: Option<(gpui::Point<Pixels>, usize)> = None;
            for y in 1..600 {
                let p = point(px(40.), px(y as f32));
                if let Some(ix) = sel.hit(p) {
                    if first.map(|(_, i)| ix < i).unwrap_or(true) {
                        first = Some((p, ix));
                    }
                    if last.map(|(_, i)| ix > i).unwrap_or(true) {
                        last = Some((p, ix));
                    }
                }
            }
            (
                first.expect("probe must hit the first block").0,
                last.expect("probe must hit the last block").0,
            )
        });
        // Sanity: the probe found two distinct points spanning the document.
        assert!(
            anchor_pt.y < caret_pt.y,
            "drag must go top-to-bottom: anchor={anchor_pt:?} caret={caret_pt:?}"
        );

        cx.simulate_mouse_down(anchor_pt, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(caret_pt, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(caret_pt, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        // Mouse-down focused the column, so the key event reaches its listener.
        cx.simulate_keystrokes("secondary-c");
        cx.run_until_parked();

        let text = cx
            .update(|_window, cx| cx.read_from_clipboard())
            .and_then(|item| item.text().map(|s| s.to_string()))
            .expect("clipboard must be populated after copy");
        assert!(
            text.contains("alpha") || text.contains("beta") || text.contains("gamma"),
            "first-paragraph text must be in the copied span: {text:?}"
        );
        assert!(
            text.contains("delta") || text.contains("epsilon") || text.contains("zeta"),
            "last-paragraph text must be in the copied span: {text:?}"
        );
        assert!(
            text.contains("let x = 1;"),
            "the code block between them must be in the copied span: {text:?}"
        );
    }

    /// A drag started AFTER the document is scrolled to the bottom still
    /// resolves a valid document index and copies text. The unscrolled drag
    /// test above keeps the scroll offset at zero (content fits); the real app
    /// pins a streaming thread to the bottom, so every block's painted bounds
    /// carry a non-zero scroll offset. This asserts the per-frame registry +
    /// `index_for_position` stay correct under that offset: a visible point
    /// near the viewport bottom hit-tests into the tail block, a drag between
    /// two visible points yields a non-collapsed range, and Cmd/Ctrl+C lands
    /// text from the visible tail (not the off-screen head).
    #[gpui::test]
    async fn drag_selects_after_scroll_to_bottom(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseButton, Render, ScrollHandle, point, px, size};
        use gpui_component::v_flex;
        // Long enough that ~24 paragraphs overflow a 300px viewport many times
        // over, so scroll-to-bottom parks the tail well off the head.
        let mut source = String::new();
        for i in 0..24 {
            if i > 0 {
                source.push_str("\n\n");
            }
            source.push_str(&format!("paragraph {i:02} body text here"));
        }
        cx.update(gpui_component::init);
        let markdown = cx.new(|_cx| {
            Markdown::new("m", &source)
                .theme(&Theme::default())
                .heading_mode(HeadingMode::Uniform)
        });

        // Host mirrors the manox message-list nesting: an external
        // `overflow_y_scroll().track_scroll()` container holds the (non-
        // scrollable) `Markdown` body. The selection listeners live on the
        // Markdown column inside; the scroll handle is ours to park at the
        // bottom the way the workspace arbitration does.
        struct Host {
            scroll: ScrollHandle,
            markdown: gpui::Entity<Markdown>,
        }
        impl Render for Host {
            fn render(
                &mut self,
                _w: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let scroll = self.scroll.clone();
                v_flex().size_full().child(
                    v_flex()
                        .id("list")
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_y_scroll()
                        .track_scroll(&scroll)
                        .child(self.markdown.clone()),
                )
            }
        }
        let (view, cx) = cx.add_window_view(|_window, _cx| Host {
            scroll: ScrollHandle::new(),
            markdown,
        });
        cx.simulate_resize(size(px(400.), px(300.)));
        cx.run_until_parked();

        // Pin to the bottom. Paired with `notify` so a paint runs in the same
        // turn and consumes the one-shot `scroll_to_bottom` flag.
        view.update(cx, |v, cx| {
            v.scroll.scroll_to_bottom();
            cx.notify();
        });
        cx.run_until_parked();
        cx.run_until_parked();

        // The visible tail: points inside the viewport (y near the bottom) must
        // resolve to real document indices. If the per-frame registry were
        // empty when scrolled, or `index_for_position` misread the scroll-
        // shifted bounds, these `hit` calls return None and the drag below
        // never begins.
        let markdown = view.read_with(cx, |v, _| v.markdown.clone());
        let (anchor_pt, caret_pt) = markdown.read_with(cx, |m, _| {
            let sel = m.selection.clone();
            let mut pts: Vec<(gpui::Point<Pixels>, usize)> = Vec::new();
            for y in 100..300 {
                let p = point(px(40.), px(y as f32));
                if let Some(ix) = sel.hit(p) {
                    pts.push((p, ix));
                }
            }
            assert!(
                pts.len() >= 2,
                "scrolled registry must hit-test the visible tail: got {} hits",
                pts.len()
            );
            (pts[0].0, pts[pts.len() - 1].0)
        });
        assert!(
            anchor_pt.y < caret_pt.y,
            "drag must go top-to-bottom: anchor={anchor_pt:?} caret={caret_pt:?}"
        );

        cx.simulate_mouse_down(anchor_pt, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(caret_pt, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(caret_pt, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        // Mouse-down focused the Markdown column, so the key event reaches its
        // listener.
        cx.simulate_keystrokes("secondary-c");
        cx.run_until_parked();

        let text = cx
            .update(|_window, cx| cx.read_from_clipboard())
            .and_then(|item| item.text().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(
            !text.is_empty(),
            "copy after a scrolled drag must land text, got empty"
        );
    }

    /// Diagnostic reproducing the manox message-list container shape exactly —
    /// `min_h_full().justify_end().overflow_y_scroll()` on the scroll div
    /// itself (not a child wrapper). Confirms whether this shape yields a
    /// scrollable viewport (max_offset grows with content) or a dead one
    /// (max_offset stuck near 0, content-sized box).
    #[gpui::test]
    async fn manox_style_container_is_scrollable(cx: &mut TestAppContext) {
        use gpui::ScrollHandle;
        struct H {
            scroll: ScrollHandle,
            n: usize,
        }
        impl Render for H {
            fn render(
                &mut self,
                _w: &mut Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let scroll = self.scroll.clone();
                use gpui_component::{h_flex, v_flex as vvf};
                // Reproduce the manox nesting: body > h_flex > list_wrap(flex_1
                // h_full) > list_el(min_h_full + justify_end + overflow_y_scroll).
                v_flex().size_full().overflow_hidden().child(
                    h_flex()
                        .flex_1()
                        .w_full()
                        .min_h_0()
                        .overflow_hidden()
                        .child(
                            vvf().flex_1().h_full().min_h_0().child(
                                v_flex()
                                    .id("list")
                                    .w_full()
                                    .min_h_full()
                                    .min_w_0()
                                    .justify_end()
                                    .overflow_y_scroll()
                                    .track_scroll(&scroll)
                                    .children((0..self.n).map(|i| {
                                        v_flex()
                                            .id(("item", i))
                                            .w_full()
                                            .h(px(60.))
                                            .flex_shrink_0()
                                            .child(format!("item {i}"))
                                    })),
                            ),
                        ),
                )
            }
        }
        let (view, cx) = cx.add_window_view(|_w, _cx| H {
            scroll: ScrollHandle::new(),
            n: 40,
        });
        cx.simulate_resize(size(px(400.), px(300.)));
        cx.run_until_parked();
        let max = view.read_with(cx, |v, _| v.scroll.max_offset().y);

        // Pin to the bottom the way the manox arbitration does on entry.
        view.update(cx, |v, cx| {
            v.scroll.scroll_to_bottom();
            cx.notify();
        });
        cx.run_until_parked();
        cx.run_until_parked();
        let off = view.read_with(cx, |v, _| v.scroll.offset().y);
        assert!(
            max > px(1000.),
            "manox-style container must be scrollable, max_offset={max:?}"
        );
        assert!(
            off <= -max + px(1.0),
            "scroll_to_bottom must reach the bottom under justify_end+min_h_full: off={off:?} -max={:?}",
            -max
        );
    }

    /// Pixel-anchored scroll survives a window resize. Mirrors the manox
    /// message-list container (`v_flex().min_h_full().justify_end().overflow_y_scroll().track_scroll()`):
    /// the scroll position is an absolute pixel offset, so when the window
    /// resizes and the content reflows, a viewport parked on history (off the
    /// bottom) must keep its pixel position — it must NOT reset to the top
    /// ("滚上天"). This is the runtime guarantee the `ScrollHandle` rebuild
    /// rests on, asserted in a real paint + real resize cycle.
    use gpui::{Render, ScrollHandle, Window, point, px, size};
    use gpui_component::v_flex;

    struct ScrollHost {
        scroll: ScrollHandle,
        n: usize,
        // Source of truth for whether streaming appends should re-pin the tail.
        // The per-frame arbitration writes it; `append` reads it — identical to
        // the message list's `auto_follow`.
        auto_follow: bool,
    }

    impl ScrollHost {
        // Append an item the way a streaming token grows the tail: re-pin to the
        // bottom only while the user was following it. `cx.notify()` schedules a
        // re-render so the new item actually enters the layout (otherwise `n`
        // grows but the list never re-paints and the assertion is vacuous).
        fn append(&mut self, cx: &mut gpui::Context<Self>) {
            self.n += 1;
            if self.auto_follow {
                self.scroll.scroll_to_bottom();
            }
            cx.notify();
        }
    }

    impl Render for ScrollHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            // Tail-follow arbitration matching the message list. gpui's scroll
            // offset is non-positive (0 at top, -max at bottom), so "at bottom"
            // means the offset has reached the negative max — not crossed the
            // positive max (which a non-positive offset can never satisfy, so a
            // `>=` comparison silently never fires and tail-follow dies whenever
            // content overflows).
            let max_y = self.scroll.max_offset().y;
            let off_y = self.scroll.offset().y;
            let at_bottom = max_y <= px(0.5) || off_y <= -max_y + px(1.0);
            self.auto_follow = at_bottom;
            if at_bottom {
                self.scroll.scroll_to_bottom();
            }
            let scroll = self.scroll.clone();
            // The scroll div is bounded to the window: the root claims the full
            // window via `size_full`, and `flex_1().min_h_0()` caps the list at
            // that height so tall content overflows the box instead of growing
            // it (an unbounded box has max_offset 0 and never scrolls).
            v_flex().size_full().child(
                v_flex()
                    .id("list")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .children((0..self.n).map(|i| {
                        v_flex()
                            .id(("item", i))
                            .w_full()
                            .h(px(60.))
                            .child(format!("item {i}"))
                    })),
            )
        }
    }

    #[gpui::test]
    async fn pixel_anchor_survives_resize(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, _cx| ScrollHost {
            scroll: ScrollHandle::new(),
            n: 40,
            auto_follow: true,
        });
        // Shrink so the 40×60px content (2400px) overflows the viewport.
        cx.simulate_resize(size(px(400.), px(300.)));
        cx.run_until_parked();
        let max_before = view.read_with(cx, |v, _| v.scroll.max_offset().y);
        assert!(
            max_before > px(0.),
            "content must overflow the viewport, max_offset={max_before:?}"
        );

        // Scroll up off the bottom — a valid non-positive offset (reading
        // history), the state the index-anchor "滚上天" bug yanked.
        view.update(cx, |v, _cx| v.scroll.set_offset(point(px(0.), px(-200.))));
        cx.run_until_parked();
        let before = view.read_with(cx, |v, _| v.scroll.offset().y);
        assert_eq!(
            before,
            px(-200.),
            "offset must hold the parked value after paint"
        );

        // Resize narrower + shorter — a width change forces full reflow, the
        // classic "滚上天" trigger under index-anchor. Pixel-anchor must hold.
        cx.simulate_resize(size(px(300.), px(250.)));
        cx.run_until_parked();
        let after = view.read_with(cx, |v, _| v.scroll.offset().y);
        assert_eq!(
            after, before,
            "pixel offset must be preserved across resize, not reset to top"
        );
    }

    /// A viewport parked at the bottom must follow the tail as streaming appends
    /// items. Two appends are needed to expose a wrong-sign arbitration: the
    /// first is survived on the initial `auto_follow = true` alone; only the
    /// second reveals whether the per-frame arbitration re-armed `auto_follow`
    /// while at the bottom. With the correct (non-positive) comparison the
    /// viewport stays pinned to the bottom across both appends.
    #[gpui::test]
    async fn streaming_follows_tail_when_at_bottom(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, _cx| ScrollHost {
            scroll: ScrollHandle::new(),
            n: 40,
            auto_follow: true,
        });
        cx.simulate_resize(size(px(400.), px(300.)));
        cx.run_until_parked();

        // Park at the bottom, then let the arbitration confirm the pin. Each
        // handle mutation is paired with `notify` so a paint runs in the same
        // turn and consumes the pending `scroll_to_bottom` flag — otherwise the
        // bare call would leave the flag to fire on a later streaming paint.
        view.update(cx, |v, cx| {
            v.scroll.scroll_to_bottom();
            cx.notify();
        });
        cx.run_until_parked();
        cx.run_until_parked();

        for _ in 0..2 {
            view.update(cx, |v, cx| v.append(cx));
            cx.run_until_parked();
        }
        let off = view.read_with(cx, |v, _| v.scroll.offset().y);
        let max = view.read_with(cx, |v, _| v.scroll.max_offset().y);
        assert!(
            max > px(0.),
            "content must overflow after appends, max_offset={max:?}"
        );
        assert!(
            off <= -max + px(1.0),
            "tail must be followed: offset {off:?} should be near bottom -{max:?}"
        );
    }

    /// A viewport scrolled up into history must NOT be yanked when streaming
    /// appends below it — the pixel offset holds. This is the hold half of the
    /// "滚上天" fix: the user reading an earlier turn is not displaced by the
    /// stream growing the tail.
    #[gpui::test]
    async fn streaming_preserves_history_when_scrolled_up(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, _cx| ScrollHost {
            scroll: ScrollHandle::new(),
            n: 40,
            auto_follow: true,
        });
        cx.simulate_resize(size(px(400.), px(300.)));
        cx.run_until_parked();

        // Pin to the bottom first so the empty/max=0 `scroll_to_bottom` flags
        // are consumed and `max_offset` is measured, then disengage into
        // history. Each handle mutation is paired with `notify` so a paint runs
        // and consumes any pending `scroll_to_bottom` flag in the same turn —
        // a bare `set_offset` schedules no frame, so a lingering flag would
        // fire on the next streaming paint and yank the viewport (a harness
        // artifact, not a property of the list).
        view.update(cx, |v, cx| {
            v.scroll.scroll_to_bottom();
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |v, cx| {
            v.scroll.set_offset(point(px(0.), px(-200.)));
            cx.notify();
        });
        cx.run_until_parked();
        let before = view.read_with(cx, |v, _| v.scroll.offset().y);
        assert_eq!(before, px(-200.));

        // Streaming appends while the user reads history.
        for _ in 0..3 {
            view.update(cx, |v, cx| v.append(cx));
            cx.run_until_parked();
        }
        let after = view.read_with(cx, |v, _| v.scroll.offset().y);
        assert_eq!(
            after, before,
            "history viewport must be preserved across streaming, not yanked"
        );
    }
}
