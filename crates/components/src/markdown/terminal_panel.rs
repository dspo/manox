//! `TerminalPanel` — a first-party selectable text panel that renders tool
//! output in a terminal-like shell: a prompt block (cwd + git status / `❯`
//! command) followed by the body, composed as one selectable document. NOT a
//! real terminal — no PTY, no grid; it reuses the markdown renderer's `Sentinel`
//! / `RichText` / `DocSelection` for drag-select + Cmd/Ctrl+C + double/triple-
//! click word/line selection across the whole panel.
//!
//! The panel renders only the body — transparent, no background tint, at the
//! same size as the thinking body — so it reads as inline selectable text in the
//! message list. The terminal chrome (a clickable titlebar that shows the
//! command summary and toggles this body) is mounted by the agent-ui layer,
//! which owns the collapse state. A persistent `Entity<TerminalPanel>` (one per
//! `ToolCallItem`, owned by the conversation) holds the selection state across
//! frames, so a drag that starts on one frame survives the re-render.
//!
//! Pagination: a finalized body renders `PAGE_SIZE` lines at a time; a "load
//! more" affordance below the body grows the window by another page. Streaming
//! bodies render the whole live output (no pagination) until they finalize,
//! when the cursor resets to the first page so the result opens at the top. The
//! panel has no internal vertical scroll — the message list scrolls the whole
//! panel — so growing the window appends lines below the current viewport
//! without jumping to the tail.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    FocusHandle, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render, SharedString, Window, div,
};
use gpui_component::{Icon, IconName, Sizable as _, Theme, h_flex, v_flex};
use vte::ansi::{Attr, Color, Handler, NamedColor};

use crate::markdown::rich_text::RichText;
use crate::markdown::selection::DocSelection;
use crate::markdown::theme::MdStyles;

use super::Sentinel;

/// Lines a finalized panel renders before the "load more" affordance. Streaming
/// panels ignore the limit and show the whole live output.
const PAGE_SIZE: usize = 20;

/// How a tool's body is rendered, decided by the agent-ui layer from the tool
/// name. Determines whether body lines carry a line-number gutter / diff
/// markers or are ANSI-colored command output.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelKind {
    /// `read_file` / `write_file`: file content with a sequential line-number
    /// gutter. The agent-ui layer pre-strips the hashline `[path#TAG]` header
    /// and `N:` prefixes, so the panel numbers the content lines 1..N.
    #[default]
    File,
    /// `edit_file`: unified diff. `+`/`-` lines are colored, `@@` hunk headers
    /// (which carry the line numbers) are accented, and `[path#TAG]` / `---`
    /// section separators are muted. No per-line gutter.
    Diff,
    /// `bash` and any other command-style output: ANSI-parsed plain lines.
    Plain,
}

/// One tool invocation's shell-like rendering surface. Owned as a persistent
/// `Entity` by the conversation so the document selection (and its focus handle)
/// survive across re-renders.
pub struct TerminalPanel {
    kind: PanelKind,
    command: Option<SharedString>,
    cwd: Option<SharedString>,
    output: String,
    git: Option<GitSummary>,
    streaming: bool,
    /// Pagination cursor: how many body lines a finalized panel renders.
    visible: usize,
    selection: DocSelection,
    focus: Option<FocusHandle>,
    styles: MdStyles,
    mono_family: SharedString,
}

impl TerminalPanel {
    /// Construct with the rendering kind, the static shell context (command +
    /// cwd), and the theme. Output is fed later via `set_output`; git status is
    /// backfilled via `set_git` once the agent-ui layer probes the workdir.
    pub fn new(
        kind: PanelKind,
        command: Option<SharedString>,
        cwd: Option<SharedString>,
        theme: &Theme,
    ) -> Self {
        Self {
            kind,
            command,
            cwd,
            output: String::new(),
            git: None,
            streaming: false,
            visible: PAGE_SIZE,
            selection: DocSelection::new(),
            focus: None,
            styles: MdStyles::from_theme(theme),
            mono_family: theme.mono_font_family.clone(),
        }
    }

    pub fn set_kind(&mut self, kind: PanelKind, cx: &mut gpui::Context<Self>) {
        self.kind = kind;
        cx.notify();
    }

    /// Replace the tool output. The document is recomposed each render (cheap;
    /// tool output is small), so the panel never holds a stale snapshot.
    pub fn set_output(&mut self, output: impl Into<String>, cx: &mut gpui::Context<Self>) {
        self.output = output.into();
        cx.notify();
    }

    /// Backfill the git summary gathered off-thread by the agent-ui layer.
    pub fn set_git(&mut self, git: Option<GitSummary>, cx: &mut gpui::Context<Self>) {
        self.git = git;
        cx.notify();
    }

    /// Mark the panel as live-streaming. On the streaming→finalized transition
    /// the pagination cursor resets to the first page so a finalized result
    /// opens at the top rather than wherever the tail left off.
    pub fn set_streaming(&mut self, streaming: bool, cx: &mut gpui::Context<Self>) {
        if self.streaming && !streaming {
            self.visible = PAGE_SIZE;
        }
        self.streaming = streaming;
        cx.notify();
    }

    /// Grow the visible window by one page, clamped to the total body length.
    /// Does not touch any scroll handle — the message list keeps its pixel
    /// offset, so the newly revealed lines appear below the current viewport.
    fn show_more(&mut self, cx: &mut gpui::Context<Self>) {
        let total = self.total_lines();
        if total > self.visible {
            self.visible = (self.visible + PAGE_SIZE).min(total);
            cx.notify();
        }
    }

    /// Total body line count for the current output. The prompt block is
    /// excluded. Drives the "load more" affordance and the pagination clamp.
    fn total_lines(&self) -> usize {
        if self.output.is_empty() {
            0
        } else {
            body_parts(&self.output).len()
        }
    }

    /// The prompt block (cwd + git markers + `❯` command echo) as text +
    /// highlight ranges. The body is appended separately in `render`.
    fn prompt_block(&self) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
        let mut c = Compose {
            s: String::new(),
            runs: Vec::new(),
        };
        let fg = self.styles.foreground;
        // Guidance text (cwd / `git:` / branch / markers / `❯`) reads upright at
        // foreground; the echoed command reads italic at foreground. The upright/
        // italic split, plus the body's muted-italic base, separates prompt
        // chrome / input / output — the runs pin the slant explicitly so the
        // body's inherited italic does not leak into the prompt.
        // Line 1: cwd + git status markers (guidance — upright, foreground).
        if let Some(cwd) = &self.cwd {
            c.push_styled(&tilde(cwd), styled(fg, false));
        }
        if let Some(git) = &self.git {
            if !c.s.is_empty() {
                c.push_styled(" git:", styled(fg, false));
            }
            if let Some(branch) = &git.branch {
                c.push_styled(branch, styled(fg, false));
            }
            for (count, sym, color) in [
                (git.modified, "*", MARKER_MODIFIED),
                (git.deleted, "✘", MARKER_DELETED),
                (git.conflict, "!", MARKER_CONFLICT),
                (git.untracked, "?", MARKER_UNTRACKED),
            ] {
                if count > 0 {
                    c.push_styled(&format!("{sym}{count}"), styled(color, false));
                }
            }
        }

        // Line 2: `❯` prompt symbol (guidance) + command echo (italic).
        if let Some(cmd) = &self.command {
            if !c.s.is_empty() {
                c.push("\n");
            }
            c.push_styled("❯ ", styled(PROMPT_GREEN, false));
            c.push_styled(cmd, styled(fg, true));
        }

        (c.s, c.runs)
    }

    /// The visible body window: plain text + highlight runs, already clipped to
    /// the pagination cursor. Per-kind parsing — ANSI for `Plain`, a line-number
    /// gutter for `File`, +/- colored diff for `Diff`. Returns the total line
    /// count and the rendered count alongside the text so the caller drives the
    /// "load more" affordance from one parse.
    ///
    /// Each branch derives `total` from the same line set it slices, so the
    /// pagination cursor ([`Self::visible_count`] → `v`) can never exceed the
    /// slice length. The `Plain` branch strips ANSI + up to two trailing
    /// newlines (one before parsing, one after), so its line count can be
    /// lower than [`body_parts`] for output ending in a blank line — computing
    /// `total` from `body_parts` here would make `parts[..v]` index out of
    /// bounds and abort the process (crash report 3F94B265).
    fn body(&self) -> (String, Vec<(Range<usize>, HighlightStyle)>, usize, usize) {
        if self.output.is_empty() {
            return (String::new(), Vec::new(), 0, 0);
        }
        match self.kind {
            PanelKind::Plain => {
                let trimmed = self.output.strip_suffix('\n').unwrap_or(&self.output);
                let (plain_owned, runs) = parse_ansi(trimmed);
                let plain = plain_owned.strip_suffix('\n').unwrap_or(&plain_owned);
                let parts: Vec<&str> = plain.split('\n').collect();
                let total = parts.len();
                let v = self.visible_count(total);
                // The visible window is a byte prefix of the stripped plain, so
                // the parsed highlight ranges clip by end-offset without re-parse.
                let visible: String = parts[..v].join("\n");
                let visible_len = visible.len();
                let clipped: Vec<_> = runs
                    .into_iter()
                    .filter_map(|(r, hl)| {
                        if r.start >= visible_len {
                            return None;
                        }
                        let end = r.end.min(visible_len);
                        (end > r.start).then_some((r.start..end, hl))
                    })
                    .collect();
                (visible, clipped, total, v)
            }
            PanelKind::File => {
                let parts = body_parts(&self.output);
                let total = parts.len();
                let v = self.visible_count(total);
                let width = digits(total);
                let muted = style_color(self.styles.muted);
                let mut s = String::new();
                let mut runs = Vec::new();
                for (i, line) in parts.iter().take(v).enumerate() {
                    let line_start = s.len();
                    let gutter = format!("{:>width$}  ", i + 1);
                    let g_len = gutter.len();
                    s.push_str(&gutter);
                    runs.push((line_start..line_start + g_len, muted));
                    s.push_str(line);
                    s.push('\n');
                }
                trim_trailing_newline(&mut s);
                (s, runs, total, v)
            }
            PanelKind::Diff => {
                let parts = body_parts(&self.output);
                let total = parts.len();
                let v = self.visible_count(total);
                let muted = self.styles.muted;
                let mut s = String::new();
                let mut runs = Vec::new();
                for line in parts.iter().take(v) {
                    let line_start = s.len();
                    s.push_str(line);
                    let color = match classify_diff(line) {
                        DiffLine::Added => Some(DIFF_ADDED),
                        DiffLine::Removed => Some(DIFF_REMOVED),
                        DiffLine::HunkHeader => Some(DIFF_HUNK),
                        DiffLine::Meta => Some(muted),
                        DiffLine::Context => None,
                    };
                    if let Some(c) = color {
                        runs.push((line_start..s.len(), style_color(c)));
                    }
                    s.push('\n');
                }
                trim_trailing_newline(&mut s);
                (s, runs, total, v)
            }
        }
    }

    /// Pagination cursor for `total` body lines: the full count while streaming,
    /// otherwise clamped to the user-expanded `visible` window. Always `<= total`,
    /// so callers slicing `parts[..v]` stay in bounds.
    fn visible_count(&self, total: usize) -> usize {
        if self.streaming {
            total
        } else {
            self.visible.min(total)
        }
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        let selection_bg = self.styles.selection_bg;
        let border = self.styles.border;
        let hover_bg = self.styles.selection_bg;
        let muted = self.styles.muted;

        let (body_text, body_runs, total, visible) = self.body();
        let has_more = !self.streaming && visible < total;
        let next = total - visible;

        // Compose the prompt block + visible body into one selectable document.
        let (mut s, mut runs) = self.prompt_block();
        if !body_text.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            let offset = s.len();
            s.push_str(&body_text);
            for (r, hl) in body_runs {
                runs.push((r.start + offset..r.end + offset, hl));
            }
        }

        // The body is the secondary, italic text: `.italic()` sets the base the
        // RichText unstyled ranges inherit (so command output / file content /
        // diff context all read muted + italic). The prompt-block guidance and
        // command runs pin their own slant via `styled`, overriding back.
        let doc = div().w_full().min_w_0().overflow_hidden().italic().child(
            RichText::new(SharedString::from(s), 0, self.selection.clone())
                .highlights(runs)
                .selection_bg(selection_bg),
        );

        let mut panel = v_flex()
            .id("terminal-panel")
            .w_full()
            .min_w_0()
            .px_3()
            .py_2()
            .text_sm()
            // The body (tool output) is the secondary text; the prompt block — the
            // input echo (cwd / git / `❯ command`) — overrides to foreground via its
            // own highlight runs, so input reads as primary and output as muted.
            .text_color(self.styles.muted)
            .font_family(self.mono_family.clone())
            .cursor_text()
            .track_focus(&focus)
            .child(Sentinel {
                selection: self.selection.clone(),
            })
            .child(doc);
        if has_more {
            panel = panel.child(
                h_flex()
                    .id("terminal-load-more")
                    .w_full()
                    .border_t_1()
                    .border_color(border)
                    .py_1()
                    .justify_center()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover_bg))
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .ml_1()
                            .child(SharedString::from(format!("+{}", next.min(PAGE_SIZE)))),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.show_more(cx))),
            );
        }
        panel
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                    if let Some(ix) = this.selection.hit(e.position) {
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
                        this.selection.end();
                        cx.notify();
                    }
                }),
            )
            .into_any_element()
    }
}

/// Git status summary for the prompt's cwd line. `branch` is `None` when the
/// cwd is not inside a git worktree, in which case only the cwd is rendered.
#[derive(Clone, Default)]
pub struct GitSummary {
    pub branch: Option<String>,
    pub modified: usize,
    pub deleted: usize,
    pub conflict: usize,
    pub untracked: usize,
}

/// Mutable accumulator for the composed document: the plain text grown left to
/// right and the highlight ranges recorded against byte offsets into that text.
struct Compose {
    s: String,
    runs: Vec<(Range<usize>, HighlightStyle)>,
}

impl Compose {
    fn push(&mut self, text: &str) {
        self.s.push_str(text);
    }

    fn push_styled(&mut self, text: &str, style: HighlightStyle) {
        let start = self.s.len();
        self.s.push_str(text);
        if style != HighlightStyle::default() {
            self.runs.push((start..self.s.len(), style));
        }
    }
}

fn style_color(c: Hsla) -> HighlightStyle {
    HighlightStyle {
        color: Some(c),
        ..Default::default()
    }
}

/// A color highlight that also pins the slant, so prompt guidance can override
/// the body's inherited italic base back to upright (and the command echo can
/// stay italic). `style_color` leaves `font_style` inherited.
fn styled(color: Hsla, italic: bool) -> HighlightStyle {
    HighlightStyle {
        color: Some(color),
        font_style: Some(if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        }),
        ..Default::default()
    }
}

/// Split `s` into body lines, dropping one trailing newline so a final `\n`
/// does not surface as a blank selectable line at the bottom of the window.
fn body_parts(s: &str) -> Vec<&str> {
    let trimmed = s.strip_suffix('\n').unwrap_or(s);
    trimmed.split('\n').collect()
}

/// Decimal digit count of `n`, floored at 1. Sizes the line-number gutter so
/// every number aligns to the widest one in the file.
fn digits(n: usize) -> usize {
    let mut n = n.max(1);
    let mut d = 0;
    while n > 0 {
        n /= 10;
        d += 1;
    }
    d.max(1)
}

fn trim_trailing_newline(s: &mut String) {
    if s.ends_with('\n') {
        s.pop();
    }
}

/// Replace the `$HOME` prefix of a cwd with `~` so the prompt line reads like a
/// shell prompt rather than an absolute path.
fn tilde(cwd: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return cwd.to_string();
    };
    let home = home.to_string_lossy();
    if cwd == home.as_ref() {
        return "~".to_string();
    }
    if cwd.starts_with(home.as_ref()) && cwd.as_bytes().get(home.len()) == Some(&b'/') {
        return format!("~{}", &cwd[home.len()..]);
    }
    cwd.to_string()
}

// --- prompt-line accent colors (fixed; legible on both light/dark panel bg) ---

const PROMPT_GREEN: Hsla = Hsla {
    h: 145. / 360.,
    s: 0.63,
    l: 0.47,
    a: 1.0,
};
const MARKER_MODIFIED: Hsla = Hsla {
    h: 40. / 360.,
    s: 0.80,
    l: 0.55,
    a: 1.0,
};
const MARKER_DELETED: Hsla = Hsla {
    h: 354. / 360.,
    s: 0.70,
    l: 0.58,
    a: 1.0,
};
const MARKER_CONFLICT: Hsla = Hsla {
    h: 354. / 360.,
    s: 0.78,
    l: 0.50,
    a: 1.0,
};
const MARKER_UNTRACKED: Hsla = Hsla {
    h: 187. / 360.,
    s: 0.55,
    l: 0.50,
    a: 1.0,
};

// --- diff marker colors ---

const DIFF_ADDED: Hsla = Hsla {
    h: 120. / 360.,
    s: 0.45,
    l: 0.55,
    a: 1.0,
};
const DIFF_REMOVED: Hsla = Hsla {
    h: 0.0,
    s: 0.55,
    l: 0.58,
    a: 1.0,
};
const DIFF_HUNK: Hsla = Hsla {
    h: 187. / 360.,
    s: 0.55,
    l: 0.50,
    a: 1.0,
};

/// One diff line's rendering class, derived from its first character(s).
#[derive(Clone, Copy)]
enum DiffLine {
    Added,
    Removed,
    HunkHeader,
    Meta,
    Context,
}

/// Classify a unified-diff line by its prefix. `@@` → hunk header (carries the
/// line numbers), `[...]` → the hashline `[path#TAG]` section header, a bare
/// `---` → the section separator, `+`/`-` → added/removed, else context.
fn classify_diff(line: &str) -> DiffLine {
    if line.starts_with("@@") {
        DiffLine::HunkHeader
    } else if line.starts_with('[') || line == "---" {
        DiffLine::Meta
    } else if line.starts_with('+') {
        DiffLine::Added
    } else if line.starts_with('-') {
        DiffLine::Removed
    } else {
        DiffLine::Context
    }
}

// --- ANSI parsing via vte ---

/// Parse ANSI escape sequences out of `output`, returning the plain text with
/// control sequences stripped and the byte ranges that carry a non-default
/// foreground color / bold / italic. Background-color SGR sequences are tracked
/// but not emitted this round (painting a background wash needs a code-span
/// overlay, deferred); the parser state still advances correctly through them.
pub fn parse_ansi(output: &str) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
    let mut proc = vte::ansi::Processor::<vte::ansi::StdSyncHandler>::new();
    let mut h = Collector::new();
    for b in output.bytes() {
        proc.advance(&mut h, b);
    }
    h.finish();
    (h.text, h.runs)
}

/// One SGR state snapshot: foreground color, bold, italic. `Default` is the
/// plain-text base (no override) so unstyled runs emit no range.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct AnsiState {
    color: Option<Hsla>,
    bold: bool,
    italic: bool,
}

impl AnsiState {
    fn to_highlight(self) -> HighlightStyle {
        HighlightStyle {
            color: self.color,
            font_weight: self.bold.then_some(FontWeight::BOLD),
            font_style: self.italic.then_some(FontStyle::Italic),
            ..Default::default()
        }
    }
}

/// vte `Handler` that strips escape sequences down to plain text while recording
/// the SGR style runs as `(byte_range, HighlightStyle)`.
struct Collector {
    text: String,
    state: AnsiState,
    /// Byte offset where the current style's run began.
    run_start: usize,
    runs: Vec<(Range<usize>, HighlightStyle)>,
}

impl Collector {
    fn new() -> Self {
        Self {
            text: String::new(),
            state: AnsiState::default(),
            run_start: 0,
            runs: Vec::new(),
        }
    }

    /// Close the run covered by the *previous* style and begin a new one at the
    /// current text tail. Only non-default styles produce a range.
    fn change(&mut self, next: AnsiState) {
        if next == self.state {
            return;
        }
        let prev = self.state.to_highlight();
        if prev != HighlightStyle::default() && self.run_start < self.text.len() {
            self.runs.push((self.run_start..self.text.len(), prev));
        }
        self.run_start = self.text.len();
        self.state = next;
    }

    /// Flush the trailing run at end-of-input.
    fn finish(&mut self) {
        let hl = self.state.to_highlight();
        if hl != HighlightStyle::default() && self.run_start < self.text.len() {
            self.runs.push((self.run_start..self.text.len(), hl));
        }
    }
}

impl Handler for Collector {
    fn input(&mut self, c: char) {
        self.text.push(c);
    }

    fn linefeed(&mut self) {
        self.text.push('\n');
    }

    fn put_tab(&mut self, _count: u16) {
        self.text.push('\t');
    }

    /// Drop `\r`; tool output uses `\r\n` and the `\n` already breaks the line.
    /// Overwrite semantics of a bare `\r` (return-to-column-0) are not modeled —
    /// the panel renders a flat transcript, not a grid.
    fn carriage_return(&mut self) {}

    fn terminal_attribute(&mut self, attr: Attr) {
        let mut next = self.state;
        apply_attr(&mut next, attr);
        self.change(next);
    }
}

/// Fold one SGR attribute into the running state.
fn apply_attr(state: &mut AnsiState, attr: Attr) {
    match attr {
        Attr::Reset => *state = AnsiState::default(),
        Attr::Bold => state.bold = true,
        Attr::Dim => {}
        Attr::Italic => state.italic = true,
        Attr::CancelBold | Attr::CancelBoldDim => state.bold = false,
        Attr::CancelItalic => state.italic = false,
        Attr::Foreground(c) => state.color = resolve_color(c),
        // Background colors, underline, reverse, strike, blink are not painted
        // this round — advancing the state through them keeps parsing correct.
        Attr::Background(_) => {}
        _ => {}
    }
}

/// Resolve a vte `Color` to a paintable foreground. `Named` default-foreground
/// variants resolve to `None` (fall back to the panel's base text color).
fn resolve_color(c: Color) -> Option<Hsla> {
    match c {
        Color::Named(n) => named_color(n),
        Color::Spec(rgb) => Some(Hsla::from(gpui::rgb(rgb_to_hex(rgb)))),
        Color::Indexed(i) => Some(indexed_color(i)),
    }
}

/// Standard 16-color palette for the named ANSI colors (Black..BrightWhite).
const PALETTE16_HEX: [u32; 16] = [
    0x000000, // Black
    0xCD0000, // Red
    0x00CD00, // Green
    0xCDCD00, // Yellow
    0x0000EE, // Blue
    0xCD00CD, // Magenta
    0x00CDCD, // Cyan
    0xE5E5E5, // White
    0x4F4F4F, // BrightBlack
    0xFF0000, // BrightRed
    0x00FF00, // BrightGreen
    0xFFFF00, // BrightYellow
    0x5C5CFF, // BrightBlue
    0xFF00FF, // BrightMagenta
    0x00FFFF, // BrightCyan
    0xFFFFFF, // BrightWhite
];

/// Named colors 0..16 take the palette; foreground/cursor/background variants
/// resolve to the panel base (no override).
fn named_color(n: NamedColor) -> Option<Hsla> {
    let d = n as u16;
    if d < 16 {
        Some(Hsla::from(gpui::rgb(PALETTE16_HEX[d as usize])))
    } else {
        None
    }
}

/// 256-color indexed: 0..15 palette, 16..231 the 6×6×6 RGB cube, 232..255 the
/// grayscale ramp.
fn indexed_color(i: u8) -> Hsla {
    if i < 16 {
        return Hsla::from(gpui::rgb(PALETTE16_HEX[i as usize]));
    }
    if i < 232 {
        let i = (i - 16) as usize;
        let r = i / 36;
        let g = (i / 6) % 6;
        let b = i % 6;
        let lvl = |c: usize| -> u32 { if c == 0 { 0 } else { 55 + c as u32 * 40 } };
        return Hsla::from(gpui::rgb((lvl(r) << 16) | (lvl(g) << 8) | lvl(b)));
    }
    let v = 8 + (i - 232) as u32 * 10;
    Hsla::from(gpui::rgb((v << 16) | (v << 8) | v))
}

/// Pack an `Rgb` triple into the `0xRRGGBB` u32 that `gpui::rgb` expects.
fn rgb_to_hex(rgb: vte::ansi::Rgb) -> u32 {
    (rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &str, start: usize, end: usize) -> (usize, usize, &str) {
        (start, end, &s[start..end])
    }

    #[test]
    fn parse_strips_control_sequences_and_keeps_text() {
        let (text, runs) = parse_ansi("\x1b[32mhello\x1b[0m world");
        assert_eq!(text, "hello world");
        assert_eq!(runs.len(), 1);
        let (s, e, slice) = run(&text, runs[0].0.start, runs[0].0.end);
        assert_eq!((s, e, slice), (0, 5, "hello"));
    }

    #[test]
    fn parse_emits_no_run_for_unstyled_text() {
        let (text, runs) = parse_ansi("plain text, no escapes");
        assert_eq!(text, "plain text, no escapes");
        assert!(runs.is_empty());
    }

    #[test]
    fn parse_tracks_bold_and_italic_separately() {
        let (text, runs) = parse_ansi("\x1b[1m bold \x1b[22m\x1b[3m italic \x1b[23m plain");
        assert_eq!(text, " bold  italic  plain");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].1.font_weight, Some(FontWeight::BOLD));
        assert_eq!(runs[1].1.font_style, Some(FontStyle::Italic));
    }

    #[test]
    fn parse_resets_style_on_sgr_0() {
        let (text, runs) = parse_ansi("\x1b[31mred\x1b[0mplain");
        assert_eq!(text, "redplain");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, 0..3);
    }

    #[test]
    fn parse_truecolor_24bit() {
        let (text, runs) = parse_ansi("\x1b[38;2;255;128;0morange\x1b[0m");
        assert_eq!(text, "orange");
        assert_eq!(runs.len(), 1);
        assert!(runs[0].1.color.is_some());
    }

    #[test]
    fn carriage_return_dropped_and_linefeed_kept() {
        let (text, _runs) = parse_ansi("a\r\nb");
        assert_eq!(text, "a\nb");
    }

    #[test]
    fn tilde_replaces_home_prefix() {
        let home = std::env::var_os("HOME").map(|s| s.to_string_lossy().into_owned());
        if let Some(home) = home {
            assert_eq!(tilde(&home), "~");
            assert_eq!(tilde(&format!("{home}/sub/dir")), "~/sub/dir");
        }
    }

    #[test]
    fn tilde_leaves_unrelated_paths() {
        assert_eq!(tilde("/usr/local/bin"), "/usr/local/bin");
    }

    #[test]
    fn digits_floor_is_one_and_grows_with_magnitude() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(5), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(99), 2);
        assert_eq!(digits(100), 3);
    }

    #[test]
    fn body_parts_drops_one_trailing_newline() {
        assert_eq!(body_parts("a\nb\nc"), vec!["a", "b", "c"]);
        assert_eq!(body_parts("a\nb\nc\n"), vec!["a", "b", "c"]);
        // Two trailing newlines keep one blank line.
        assert_eq!(body_parts("a\nb\n\n"), vec!["a", "b", ""]);
    }

    fn panel_body(
        kind: PanelKind,
        output: &str,
        visible: usize,
        streaming: bool,
    ) -> (String, usize, usize) {
        let theme = test_theme();
        let mut p = TerminalPanel::new(kind, None, None, &theme);
        p.streaming = streaming;
        p.visible = visible;
        p.output = output.to_string();
        let (text, _runs, total, v) = p.body();
        (text, total, v)
    }

    fn test_theme() -> Theme {
        // A minimal theme is not available without gpui_component init; the
        // numeric helpers below avoid rendering and so skip the theme entirely.
        Theme::default()
    }

    #[test]
    fn file_gutter_numbers_lines_sequentially() {
        // Five lines, page size 3 → first three lines, numbered 1..3.
        let (text, total, v) = panel_body(
            PanelKind::File,
            "alpha\nbeta\ngamma\ndelta\nepsilon",
            3,
            false,
        );
        assert_eq!(total, 5);
        assert_eq!(v, 3);
        assert!(text.contains("1  alpha"));
        assert!(text.contains("2  beta"));
        assert!(text.contains("3  gamma"));
        assert!(!text.contains("delta"));
    }

    #[test]
    fn plain_clips_to_visible_lines_and_keeps_ansi_text() {
        // ANSI red over "two"; the stripped plain is "one\ntwo\nthree", clipped
        // to 2 lines.
        let (text, total, v) =
            panel_body(PanelKind::Plain, "one\n\x1b[31mtwo\x1b[0m\nthree", 2, false);
        assert_eq!(total, 3);
        assert_eq!(v, 2);
        assert_eq!(text, "one\ntwo");
    }

    #[test]
    fn streaming_renders_all_lines_ignoring_cursor() {
        let (_text, total, v) = panel_body(PanelKind::File, "a\nb\nc\nd\ne", 2, true);
        // Streaming overrides the cursor: all 5 lines visible.
        assert_eq!(total, 5);
        assert_eq!(v, 5);
    }

    #[test]
    fn plain_trailing_blank_line_does_not_panic_streaming() {
        // Regression for crash report 3F94B265: output ending in a blank line
        // ("\n\n") made `parts[..v]` index out of bounds because `total` (from
        // `body_parts`, which strips one trailing newline) exceeded `parts.len()`
        // (the Plain branch strips up to two). `total` is now derived from the
        // same `parts` that get sliced, so streaming shows the single real line.
        let (text, total, v) = panel_body(PanelKind::Plain, "a\n\n", 0, true);
        assert_eq!(total, 1);
        assert_eq!(v, 1);
        assert_eq!(text, "a");
    }

    #[test]
    fn plain_trailing_blank_line_clamps_pagination_cursor() {
        // Non-streaming with `visible` past the real line count must clamp, not
        // panic. The trailing blank line is not a renderable line, so total is 2.
        let (text, total, v) = panel_body(PanelKind::Plain, "a\nb\n\n", 10, false);
        assert_eq!(total, 2);
        assert_eq!(v, 2);
        assert_eq!(text, "a\nb");
    }

    #[test]
    fn diff_classifies_lines_by_prefix() {
        assert!(matches!(
            classify_diff("@@ -1,3 +1,4 @@"),
            DiffLine::HunkHeader
        ));
        assert!(matches!(classify_diff("[src/main.rs#abc]"), DiffLine::Meta));
        assert!(matches!(classify_diff("---"), DiffLine::Meta));
        assert!(matches!(classify_diff("+added"), DiffLine::Added));
        assert!(matches!(classify_diff("-removed"), DiffLine::Removed));
        assert!(matches!(classify_diff(" context"), DiffLine::Context));
        assert!(matches!(classify_diff(""), DiffLine::Context));
    }
}
