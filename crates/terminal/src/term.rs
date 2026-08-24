//! `Terminal` Entity — the gpui state machine wrapping an alacritty `Term`.
//!
//! `Terminal` owns an `Arc<FairMutex<ManoxTerm>>` (the alacritty grid/ANSI
//! engine), a `Box<dyn PtySource>`, and a gpui task that drains the event
//! channel: `PtyOutput` is fed back into the Term under the lock; the rest are
//! re-emitted via `EventEmitter<TerminalEvent>` for the view layer.
//!
//! The Term lock is taken only on the gpui side. The PTY reader/writer
//! threads never touch it — they move raw bytes over the channel.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vi_mode::ViMotion;
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle, Processor, StdSyncHandler};
use anyhow::Result;
use gpui::{App, AppContext as _, AsyncApp, ClipboardItem, Context, Entity, EventEmitter, Task};

use crate::event::{ManoxListener, TerminalEvent};
use crate::pty_source::PtySource;
use crate::readiness::{ReadinessMode, ReadinessTracker};
use crate::settings::{
    BellMode, CursorBlinkSetting, CursorShapeSetting, Osc52Access, TerminalSettings,
};
use crate::tap::{OscTap, TapEvent};

pub(crate) type ManoxTerm = Term<ManoxListener>;
pub(crate) type ManoxTermLock = FairMutex<ManoxTerm>;

/// A hoverable target on a single visible row: the text, its display row and
/// column span (inclusive), and how to open it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverTarget {
    pub text: String,
    /// Visible display row (0 = topmost visible line) — paint coordinates.
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
    pub kind: HoverKind,
}

/// How a hovered span opens: URLs in the browser, paths revealed in the
/// file manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoverKind {
    Url,
    Path,
}

/// Grid dimensions supplied to `Term::new` / `Term::resize`.
#[derive(Copy, Clone)]
pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Build the alacritty `Config` from `[terminal]` settings: scrollback size,
/// cursor glyph, and OSC 52 policy. Alacritty gates OSC 52 internally per
/// `Config.osc52`, so the gpui task only sees allowed clipboard requests.
/// Kitty keyboard mode is honored so programs can disambiguate modified keys
/// (shift+enter); with the config flag off the Term ignores the mode and the
/// key encoder never sees it activate.
fn build_config(settings: &TerminalSettings) -> Config {
    Config {
        scrolling_history: settings.scrolling_history,
        default_cursor_style: map_cursor(settings.cursor_shape, settings.cursor_blink),
        osc52: map_osc52(settings.osc52_access),
        kitty_keyboard: true,
        // Drop `:` from the default separator set so URLs and `host:port`
        // text stay one semantic word (double-click select, hover target).
        semantic_escape_chars: ",│`\"' ()[]{}<>\t".into(),
        ..Config::default()
    }
}

/// The default style only seeds the Term — programs override shape and blink
/// via DECSCUSR / DECSET 12. It blinks under `cursor_blink = "on"`;
/// `"terminal"` leaves the flag to the program.
fn map_cursor(s: CursorShapeSetting, blink: CursorBlinkSetting) -> CursorStyle {
    let shape = match s {
        CursorShapeSetting::Block => CursorShape::Block,
        CursorShapeSetting::Underline => CursorShape::Underline,
        CursorShapeSetting::Beam => CursorShape::Beam,
    };
    CursorStyle {
        shape,
        blinking: matches!(blink, CursorBlinkSetting::On),
    }
}

fn map_osc52(a: Osc52Access) -> Osc52 {
    match a {
        Osc52Access::Allow => Osc52::CopyPaste,
        Osc52Access::Deny => Osc52::Disabled,
    }
}

pub struct Terminal {
    pub id: String,
    pub cwd: PathBuf,
    pub cols: usize,
    pub rows: usize,
    term: Arc<ManoxTermLock>,
    pty: Box<dyn PtySource>,
    output_processor: Processor<StdSyncHandler>,
    /// Byte tap observing the PTY stream for the readiness marker and OSC 7
    /// cwd reports, parallel to the vte processor.
    tap: OscTap,
    readiness: ReadinessTracker,
    pub child_exited: Option<i32>,
    pub title: Option<String>,
    /// Bell policy — the view reads this to decide whether to flash / beep.
    pub bell: BellMode,
    _task: Option<Task<()>>,
    _readiness_task: Option<Task<()>>,
}

impl EventEmitter<TerminalEvent> for Terminal {}

impl Terminal {
    /// Create a Terminal running the given `pty` source in `cwd`. Font,
    /// scrollback, cursor, bell, and OSC 52 policy come from `[terminal]` in
    /// settings.toml; the PTY itself (shell, env) is supplied by the caller via
    /// the `PtySource`. The source is started here — its reader / waiter
    /// threads begin emitting events onto the channel the gpui task drains.
    pub fn new(
        id: String,
        cwd: PathBuf,
        cols: usize,
        rows: usize,
        mut pty: Box<dyn PtySource>,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let settings = crate::settings::load();
        let (event_tx, event_rx) = async_channel::bounded::<TerminalEvent>(256);
        let listener = ManoxListener::new(event_tx.clone());
        let cfg = build_config(&settings);
        let size = TermSize { cols, rows };
        let term = Arc::new(FairMutex::new(Term::new(cfg, &size, listener)));
        let bell = settings.bell;

        let ready_nonce = pty.ready_nonce().map(str::to_owned);
        let readiness_mode = if ready_nonce.is_some() {
            ReadinessMode::Marker
        } else {
            ReadinessMode::Heuristic
        };

        // Move the reader fd / child handle into the source's reader / waiter
        // threads before the gpui task drains the channel.
        pty.start(event_tx.clone());

        let entity = cx.new(|cx| {
            let task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                let rx = event_rx;
                while let Ok(ev) = rx.recv().await {
                    match ev {
                        TerminalEvent::PtyOutput(bytes) => {
                            let _ = this
                                .update(cx, |t: &mut Terminal, cx| t.write_pty_output(&bytes, cx));
                        }
                        TerminalEvent::ChildExit(code) => {
                            let _ = this.update(cx, |t: &mut Terminal, cx| {
                                t.child_exited = Some(code);
                                cx.emit(TerminalEvent::ChildExit(code));
                                cx.notify();
                            });
                        }
                        TerminalEvent::Title(title) => {
                            let _ = this.update(cx, |t: &mut Terminal, cx| {
                                t.title = title.clone();
                                cx.emit(TerminalEvent::Title(title));
                                cx.notify();
                            });
                        }
                        // OSC 52 write: store text on the system clipboard.
                        TerminalEvent::ClipboardStore(text) => {
                            let _ = this.update(cx, |_t: &mut Terminal, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            });
                        }
                        // OSC 52 read: load the clipboard, let the TUI's
                        // callback format its response, write that back to the
                        // PTY so the application can read it.
                        TerminalEvent::ClipboardLoad(cb) => {
                            let _ = this.update(cx, |t: &mut Terminal, cx| {
                                let text = cx
                                    .read_from_clipboard()
                                    .and_then(|i| i.text())
                                    .unwrap_or_default();
                                let response = cb(&text);
                                let _ = t.input(response.as_bytes());
                            });
                        }
                        // Bytes the TUI emitted via the terminal (rare; e.g.
                        // some DCS responses). Forward to the PTY verbatim.
                        TerminalEvent::PtyWrite(text) => {
                            let _ = this.update(cx, |t: &mut Terminal, _cx| {
                                let _ = t.input(text.as_bytes());
                            });
                        }
                        other => {
                            let _ = this.update(cx, |_t: &mut Terminal, cx| {
                                cx.emit(other);
                                cx.notify();
                            });
                        }
                    }
                }
            });
            let readiness_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                // Fallback / heuristic transitions need a clock even when no
                // output arrives; marker hits transition in write_pty_output.
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(100))
                        .await;
                    let ready = this.update(cx, |t: &mut Terminal, cx| {
                        if t.readiness.poll(Instant::now()) {
                            t.emit_ready(cx);
                        }
                        t.readiness.is_ready()
                    });
                    match ready {
                        Ok(true) | Err(_) => break,
                        Ok(false) => {}
                    }
                }
            });
            Self {
                id,
                cwd,
                cols,
                rows,
                term,
                pty,
                output_processor: Processor::<StdSyncHandler>::new(),
                tap: OscTap::new(ready_nonce),
                readiness: ReadinessTracker::new(readiness_mode, Instant::now()),
                child_exited: None,
                title: None,
                bell,
                _task: Some(task),
                _readiness_task: Some(readiness_task),
            }
        });
        Ok(entity)
    }

    /// Feed PTY output through the vte processor into the Term, then nudge the
    /// view to repaint. Called only from the gpui task.
    fn write_pty_output(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        for ev in self.tap.feed(bytes) {
            match ev {
                TapEvent::ReadyMarker => {
                    if self.readiness.on_marker() {
                        self.emit_ready(cx);
                    }
                }
                TapEvent::Cwd(path) => {
                    if self.cwd != path {
                        self.cwd = path.clone();
                        cx.emit(TerminalEvent::CwdChanged(path));
                    }
                }
            }
        }
        self.readiness.on_output(Instant::now());
        let mut term = self.term.lock();
        for &b in bytes {
            self.output_processor.advance(&mut *term, b);
        }
        drop(term);
        cx.notify();
    }

    /// Whether the shell finished init and accepts input — marker tap, quiet
    /// window, or fallback timeout, whichever came first. Drives the view's
    /// starting indicator.
    pub fn is_ready(&self) -> bool {
        self.readiness.is_ready()
    }

    /// Broadcast the readiness transition. Callers guard on the tracker, so
    /// this fires at most once per terminal.
    fn emit_ready(&mut self, cx: &mut Context<Self>) {
        cx.emit(TerminalEvent::Ready);
        cx.notify();
    }

    /// Send input bytes (keystrokes, paste) to the shell.
    pub fn input(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.pty.write(bytes)
    }

    /// Name of the process owning the foreground process group, when it is
    /// not the shell itself. The view polls this on a slow timer for its
    /// foreground-process chip; `None` hides the chip (idle prompt, or the
    /// source cannot tell).
    pub fn foreground_process_name(&self) -> Option<String> {
        self.pty.foreground_process_name()
    }

    /// Resize both the PTY and the Term. No-op if unchanged.
    pub fn resize(&mut self, cols: usize, rows: usize, cx: &mut Context<Self>) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        let _ = self.pty.resize(cols as u16, rows as u16);
        let mut term = self.term.lock();
        term.resize(TermSize { cols, rows });
        drop(term);
        self.cols = cols;
        self.rows = rows;
        cx.notify();
    }

    /// Read-only access to the alacritty Term for snapshot/render paths.
    pub fn with_term<R>(&self, f: impl FnOnce(&ManoxTerm) -> R) -> R {
        let term = self.term.lock();
        f(&term)
    }

    /// Mutable access to the alacritty Term — for selection/scroll writes.
    fn with_term_mut<R>(&self, f: impl FnOnce(&mut ManoxTerm) -> R) -> R {
        let mut term = self.term.lock();
        f(&mut term)
    }

    /// Current terminal mode flags — callers (key/mouse mapping) branch on
    /// `APP_CURSOR`, `BRACKETED_PASTE`, mouse modes, etc.
    pub fn mode(&self) -> TermMode {
        self.with_term(|t| *t.mode())
    }

    /// Scroll the scrollback view by `delta` lines (negative = up into
    /// history). The alt screen has no scrollback, so this is a no-op there.
    pub fn scroll(&self, delta: i32, cx: &mut Context<Self>) {
        self.with_term_mut(|t| t.scroll_display(Scroll::Delta(delta)));
        cx.notify();
    }

    /// Scroll the display to the offset implied by a scrollbar drag at
    /// `fraction` down the track (0 = top / oldest scrollback, 1 = bottom /
    /// live edge). alacritty clamps the resulting offset to the history.
    pub fn scroll_to_fraction(&self, fraction: f32, cx: &mut Context<Self>) {
        self.with_term_mut(|t| {
            let history = t.grid().history_size();
            let target = ((1. - fraction.clamp(0., 1.)) * history as f32).round() as i32;
            let current = t.grid().display_offset() as i32;
            t.scroll_display(Scroll::Delta(target - current));
        });
        cx.notify();
    }

    /// Forward a mouse-wheel scroll to the PTY as xterm mouse reports, so a TUI
    /// app that captures the mouse (claude code / vim / htop) scrolls its own
    /// viewport instead of the (no-op, alt-screen) local scrollback. `delta_lines`
    /// is signed (negative = wheel up, positive = wheel down); one report per
    /// line, capped at a small burst so a single fling does not flood the PTY.
    /// `row`/`col` are the visible grid coords under the cursor. No-op when no
    /// mouse mode is active — callers should fall back to [`Self::scroll`].
    pub fn mouse_wheel(
        &self,
        row: usize,
        col: usize,
        delta_lines: i32,
        modifiers: &gpui::Modifiers,
    ) {
        if delta_lines == 0 {
            return;
        }
        let mode = self.mode();
        if !mode.intersects(TermMode::MOUSE_MODE) {
            return;
        }
        // xterm mouse modifier bits: shift=4, alt=8, control=16 (added to the
        // button code). Wheel up is button 64, wheel down 65.
        let mod_bits = 4 * (modifiers.shift as u8)
            + 8 * (modifiers.alt as u8)
            + 16 * (modifiers.control as u8);
        let base = if delta_lines < 0 { 64 } else { 65 };
        let count = delta_lines.unsigned_abs().min(6) as usize;
        let button = base + mod_bits;
        let report = mouse_report_bytes(mode, button, row, col);
        for _ in 0..count {
            let _ = self.pty.write(&report);
        }
    }

    /// xterm alternateScroll: with the alt screen active but no mouse capture
    /// (less, git log), wheel deltas become arrow-key presses so the program
    /// scrolls its own content. No-op when the program disabled the mode via
    /// DECRST 1007. `delta_lines` shares the wheel sign convention (negative
    /// = up); capped per event like mouse reports.
    pub fn alternate_scroll(&self, delta_lines: i32) {
        if delta_lines == 0 {
            return;
        }
        let mode = self.mode();
        if !mode.intersects(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            || mode.intersects(TermMode::MOUSE_MODE)
        {
            return;
        }
        let _ = self.pty.write(&alternate_scroll_bytes(mode, delta_lines));
    }

    /// Selected text as a plain string, if a selection is active.
    pub fn selection_to_string(&self) -> Option<String> {
        self.with_term(|t| t.selection_to_string())
    }

    pub fn clear_selection(&self) {
        self.with_term_mut(|t| t.selection = None);
    }

    /// Begin a selection of granularity `ty` at `(row, col)` in visible
    /// display coordinates (click count 1/2/3 → Simple/Semantic/Lines).
    /// `row` 0 is the visible top line. Semantic/Lines expansion happens in
    /// alacritty's `Selection::to_range`, so drags keep the granularity.
    pub fn start_selection(
        &self,
        ty: SelectionType,
        row: usize,
        col: usize,
        cx: &mut Context<Self>,
    ) {
        self.with_term_mut(|t| {
            let point = self.display_point(t, row, col);
            t.selection = Some(Selection::new(ty, point, Side::Left));
        });
        cx.notify();
    }

    /// Extend the existing selection to `(row, col)`. No-op if no selection.
    pub fn update_selection(&self, row: usize, col: usize, cx: &mut Context<Self>) {
        self.with_term_mut(|t| {
            if t.selection.is_none() {
                return;
            }
            let point = self.display_point(t, row, col);
            if let Some(sel) = t.selection.as_mut() {
                sel.update(point, Side::Right);
            }
        });
        cx.notify();
    }

    /// Map a visible `(row, col)` to an alacritty grid `Point`. alacritty
    /// numbers grid lines top-down (line 0 = topmost visible line when the
    /// display offset is 0), so grid_line = display_row - display_offset.
    fn display_point(&self, term: &ManoxTerm, row: usize, col: usize) -> Point {
        let offset = term.grid().display_offset() as i32;
        let line = row as i32 - offset;
        Point::new(Line(line), Column(col))
    }

    /// Paste text, wrapping in bracketed-paste markers when the mode is set.
    pub fn paste(&self, text: &str) -> std::io::Result<()> {
        let mode = self.mode();
        let bytes = if mode.contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{}\x1b[201~", text).into_bytes()
        } else {
            text.as_bytes().to_vec()
        };
        self.pty.write(&bytes)
    }

    /// Toggle the terminal's built-in vi mode (alacritty's, not the `vim`
    /// process) — used for keyboard-driven selection/scrollback navigation.
    pub fn toggle_vi_mode(&self, cx: &mut Context<Self>) {
        self.with_term_mut(|t| t.toggle_vi_mode());
        cx.notify();
    }

    /// Apply a vi motion. Only meaningful while vi mode is on.
    pub fn vi_motion(&self, motion: ViMotion, cx: &mut Context<Self>) {
        self.with_term_mut(|t| t.vi_motion(motion));
        cx.notify();
    }

    /// The OSC 8 hyperlink URI at `(row, col)`, if any.
    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<String> {
        self.with_term(|t| {
            let content = t.renderable_content();
            let mut display_line = -1i32;
            let mut prev: Option<i32> = None;
            for idx in content.display_iter {
                let line = idx.point.line.0;
                if prev != Some(line) {
                    display_line += 1;
                    prev = Some(line);
                }
                if display_line == row as i32
                    && idx.point.column.0 == col
                    && let Some(h) = idx.cell.hyperlink()
                {
                    return Some(h.uri().to_owned());
                }
            }
            None
        })
    }

    /// Whether the program currently wants the cursor blinking (DECSET 12 /
    /// DECSCUSR). The view's blink manager reads this under
    /// `cursor_blink = "terminal"`.
    pub fn cursor_blinking(&self) -> bool {
        self.with_term(|t| t.cursor_style().blinking)
    }

    /// The hoverable target at visible `(row, col)`: an OSC 8 hyperlink
    /// first, else the semantic word when it classifies as a URL or a path.
    /// `None` outside the visible grid or on plain text.
    pub fn hover_target(&self, row: usize, col: usize) -> Option<HoverTarget> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.with_term(|t| {
            let point = self.display_point(t, row, col);
            hyperlink_span(t, point).or_else(|| word_target(t, point))
        })
        .map(|mut h| {
            h.row = row;
            h
        })
    }

    /// All regex matches in the visible+scrollback grid, as `(start, end)`
    /// grid points. The UI overlays highlight from these.
    pub fn search_matches(&self, pattern: &str) -> Result<Vec<(Point, Point)>, String> {
        let mut regex = RegexSearch::new(pattern).map_err(|e| e.to_string())?;
        let matches = self.with_term(|t| {
            let mut out = Vec::new();
            // Start at the grid's topmost line so scrollback above the visible
            // window is searched too. alacritty numbers lines top-down, so the
            // topmost line is the most negative (oldest scrollback) line.
            let mut origin = Point::new(t.grid().topmost_line(), Column(0));
            let mut guard = 0usize;
            while let Some(m) =
                t.search_next(&mut regex, origin, Direction::Right, Side::Left, None)
            {
                let start = *m.start();
                let end = *m.end();
                out.push((start, end));
                // Advance past the match; break on zero-width to avoid loops.
                if end <= origin {
                    break;
                }
                origin = end;
                guard += 1;
                if guard > 4096 {
                    break;
                }
            }
            out
        });
        Ok(matches)
    }
}

/// Encode an xterm mouse report for `button` at visible grid `(row, col)`,
/// following the mode the TUI enabled:
/// - SGR (`\x1b[<`): `\x1b[<button;col+1;row+1M` (1-based, no +32 offset).
/// - Legacy / UTF8 (`\x1b[M`): `\x1b[M` + three payload bytes, each `32 +
///   value` (button code, 1-based column, 1-based row). Wheel button codes
///   (64/65 + modifiers) stay below 128, so the encoding is byte-identical
///   across legacy and UTF8 for the wheel case.
fn mouse_report_bytes(mode: TermMode, button: u8, row: usize, col: usize) -> Vec<u8> {
    if mode.contains(TermMode::SGR_MOUSE) {
        format!("\x1b[<{button};{};{}M", col + 1, row + 1).into_bytes()
    } else {
        let cb = (32u32 + button as u32).min(255) as u8;
        let cx = (32u32 + col as u32 + 1).min(255) as u8;
        let cy = (32u32 + row as u32 + 1).min(255) as u8;
        vec![0x1b, b'[', b'M', cb, cx, cy]
    }
}

/// One alternateScroll wheel event as arrow-key bytes: up for negative
/// deltas, SS3 (`\x1bOA`/`\x1bOB`) under APP_CURSOR, CSI otherwise. One
/// press per line, capped at 6 like mouse wheel reports.
fn alternate_scroll_bytes(mode: TermMode, delta_lines: i32) -> Vec<u8> {
    let seq: &[u8] = match (mode.contains(TermMode::APP_CURSOR), delta_lines < 0) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1bOB",
        (false, true) => b"\x1b[A",
        (false, false) => b"\x1b[B",
    };
    let mut out = Vec::with_capacity(seq.len() * 6);
    for _ in 0..delta_lines.unsigned_abs().min(6) {
        out.extend_from_slice(seq);
    }
    out
}

/// The OSC 8 hyperlink span covering `point`, if the cell carries one. The
/// span walks outward while adjacent cells carry the same URI; the hovered
/// text shown in the tooltip is the URI itself.
fn hyperlink_span(term: &ManoxTerm, point: Point) -> Option<HoverTarget> {
    let grid = term.grid();
    let row = &grid[point.line];
    let uri = row[point.column].hyperlink()?.uri().to_owned();
    let same = |c: Column| row[c].hyperlink().is_some_and(|h| h.uri() == uri);
    let mut start = point.column.0;
    while start > 0 && same(Column(start - 1)) {
        start -= 1;
    }
    let mut end = point.column.0;
    while end + 1 < grid.columns() && same(Column(end + 1)) {
        end += 1;
    }
    Some(HoverTarget {
        text: uri,
        // Stamped with the display row by `hover_target`.
        row: 0,
        start_col: start,
        end_col: end,
        kind: HoverKind::Url,
    })
}

/// The semantic word at `point` when it looks like a URL or a path.
/// Multi-line spans (wrapped words) are not hoverable.
fn word_target(term: &ManoxTerm, point: Point) -> Option<HoverTarget> {
    let start = term.semantic_search_left(point);
    let end = term.semantic_search_right(point);
    if start.line != point.line || end.line != point.line {
        return None;
    }
    let grid = term.grid();
    let row = &grid[point.line];
    let mut text = String::new();
    for c in start.column.0..=end.column.0 {
        text.push(row[Column(c)].c);
    }
    // Trailing padding / wide-char spacers are not part of the word.
    let text = text.trim_end().to_owned();
    let kind = classify_word(&text)?;
    Some(HoverTarget {
        start_col: start.column.0,
        end_col: start.column.0 + text.len().saturating_sub(1),
        // Stamped with the display row by `hover_target`.
        row: 0,
        text,
        kind,
    })
}

/// Classify a semantic word as an openable target: `http(s)://` → URL;
/// anything containing a path separator → path.
fn classify_word(text: &str) -> Option<HoverKind> {
    if text.starts_with("http://") || text.starts_with("https://") {
        return Some(HoverKind::Url);
    }
    if text.contains('/') {
        return Some(HoverKind::Path);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn grid_text(term: &ManoxTerm, rows: usize, cols: usize) -> String {
        let grid = term.grid();
        let mut s = String::new();
        for line in 0..rows {
            for col in 0..cols {
                s.push(grid[Line(line as i32)][Column(col)].c);
            }
        }
        s
    }

    /// End-to-end PTY+Term loop without the gpui Entity: spawn the default
    /// shell, write `echo hello`, drain PTY output into the Term, and assert
    /// the grid surfaces "hello". Verifies the alacritty Term + portable-pty
    /// wiring before the rendering layer lands.
    #[test]
    fn pty_echo_roundtrip() {
        let (event_tx, event_rx) = async_channel::bounded::<TerminalEvent>(256);
        let listener = ManoxListener::new(event_tx.clone());
        let cfg = Config::default();
        let size = TermSize { cols: 80, rows: 24 };
        let term = Arc::new(FairMutex::new(Term::new(cfg, &size, listener)));
        let mut pty =
            crate::pty::open(&PathBuf::from("/tmp"), 80, 24, None, &[]).expect("open pty");
        pty.start(event_tx.clone());

        // Let the shell start, then send a command.
        std::thread::sleep(Duration::from_millis(150));
        pty.write(b"echo hello\r").expect("write input");

        let mut processor = Processor::<StdSyncHandler>::new();
        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(8) {
                panic!(
                    "timeout waiting for echo output; grid:\n{}",
                    grid_text(&term.lock(), 24, 80)
                );
            }
            while let Ok(ev) = event_rx.try_recv() {
                if let TerminalEvent::PtyOutput(bytes) = ev {
                    let mut t = term.lock();
                    for &b in &bytes {
                        processor.advance(&mut *t, b);
                    }
                }
            }
            if grid_text(&term.lock(), 24, 80).contains("hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Drop kills the child and detaches both threads.
        drop(pty);
    }

    /// End-to-end readiness handshake: spawn `/bin/sh` through the marker
    /// wrapper and assert the tap sees the marker within 8s. Also asserts the
    /// env injection identifies manox as the host terminal.
    #[test]
    fn readiness_marker_and_env_roundtrip() {
        let (event_tx, event_rx) = async_channel::bounded::<TerminalEvent>(256);
        let listener = ManoxListener::new(event_tx.clone());
        let cfg = Config::default();
        let size = TermSize { cols: 80, rows: 24 };
        let term = Arc::new(FairMutex::new(Term::new(cfg, &size, listener)));
        let mut pty = crate::pty::open(&PathBuf::from("/tmp"), 80, 24, Some("/bin/sh"), &[])
            .expect("open pty");
        let nonce = pty.ready_nonce().map(str::to_owned);
        assert!(nonce.is_some(), "/bin/sh must spawn marker-wrapped");
        let mut tap = OscTap::new(nonce);
        pty.start(event_tx.clone());

        let mut processor = Processor::<StdSyncHandler>::new();
        let start = Instant::now();
        let mut marker_seen = false;
        while !marker_seen {
            if start.elapsed() > Duration::from_secs(8) {
                panic!("timeout waiting for readiness marker");
            }
            while let Ok(ev) = event_rx.try_recv() {
                if let TerminalEvent::PtyOutput(bytes) = ev {
                    if tap.feed(&bytes).contains(&TapEvent::ReadyMarker) {
                        marker_seen = true;
                    }
                    let mut t = term.lock();
                    for &b in &bytes {
                        processor.advance(&mut *t, b);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // The wrapper exec'd the real shell; env injection carries the
        // claimed terminal identity.
        pty.write(b"echo TERM_IS=$TERM_PROGRAM\r")
            .expect("write input");
        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(8) {
                panic!(
                    "timeout waiting for echo output; grid:\n{}",
                    grid_text(&term.lock(), 24, 80)
                );
            }
            while let Ok(ev) = event_rx.try_recv() {
                if let TerminalEvent::PtyOutput(bytes) = ev {
                    let mut t = term.lock();
                    for &b in &bytes {
                        processor.advance(&mut *t, b);
                    }
                }
            }
            if grid_text(&term.lock(), 24, 80).contains("TERM_IS=iTerm.app") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(pty);
    }

    #[test]
    fn sgr_wheel_report_is_one_based() {
        // wheel down = button 65 at row 2, col 4 (0-based) → 1-based 3,5.
        let b = mouse_report_bytes(TermMode::SGR_MOUSE, 65, 2, 4);
        assert_eq!(b, b"\x1b[<65;5;3M");
    }

    #[test]
    fn legacy_wheel_report_adds_32_offset() {
        // wheel up = button 64 at row 0, col 0 → payload 96 (0x60), 33 ('!'), 33.
        let b = mouse_report_bytes(TermMode::MOUSE_REPORT_CLICK, 64, 0, 0);
        assert_eq!(b, b"\x1b[M\x60!!");
    }

    #[test]
    fn alternate_scroll_csi_and_ss3() {
        // Plain mode: CSI arrows, one press per line; APP_CURSOR: SS3.
        assert_eq!(
            alternate_scroll_bytes(TermMode::empty(), 2),
            b"\x1b[B\x1b[B"
        );
        assert_eq!(alternate_scroll_bytes(TermMode::APP_CURSOR, -1), b"\x1bOA");
    }

    #[test]
    fn alternate_scroll_caps_at_six() {
        assert_eq!(alternate_scroll_bytes(TermMode::empty(), 100).len(), 6 * 3);
        assert!(alternate_scroll_bytes(TermMode::empty(), 0).is_empty());
    }

    /// OSC 10;? makes the Term raise a ColorRequest for the default
    /// foreground (index 256) through the listener.
    #[test]
    fn osc_color_query_raises_color_request() {
        let (event_tx, event_rx) = async_channel::bounded::<TerminalEvent>(256);
        let listener = ManoxListener::new(event_tx);
        let cfg = Config::default();
        let size = TermSize { cols: 80, rows: 24 };
        let mut term = Term::new(cfg, &size, listener);
        let mut processor = Processor::<StdSyncHandler>::new();
        for &b in b"\x1b]10;?\x07" {
            processor.advance(&mut term, b);
        }
        let mut got = None;
        while let Ok(ev) = event_rx.try_recv() {
            if let TerminalEvent::ColorRequest(idx, _) = ev {
                got = Some(idx);
            }
        }
        assert_eq!(got, Some(256));
    }

    /// A standalone Term fed with `text` — hover/word tests without a PTY.
    fn term_with(text: &str) -> ManoxTerm {
        let (event_tx, _rx) = async_channel::bounded::<TerminalEvent>(256);
        let listener = ManoxListener::new(event_tx);
        let cfg = build_config(&TerminalSettings::default());
        let size = TermSize { cols: 80, rows: 24 };
        let mut term = Term::new(cfg, &size, listener);
        let mut processor = Processor::<StdSyncHandler>::new();
        for &b in text.as_bytes() {
            processor.advance(&mut term, b);
        }
        term
    }

    #[test]
    fn classify_word_rules() {
        assert_eq!(classify_word("https://a.b/c"), Some(HoverKind::Url));
        assert_eq!(classify_word("http://a.b"), Some(HoverKind::Url));
        assert_eq!(classify_word("/tmp/x"), Some(HoverKind::Path));
        assert_eq!(classify_word("src/main.rs"), Some(HoverKind::Path));
        assert_eq!(classify_word("hello"), None);
    }

    #[test]
    fn word_target_classifies_url_and_path() {
        // "open https://example.com/x then /tmp/foo.txt": url cols 5..=25,
        // path cols 32..=43 — `:` stays inside the word per build_config's
        // separator set.
        let term = term_with("open https://example.com/x then /tmp/foo.txt\r\n");
        let url = word_target(&term, Point::new(Line(0), Column(8))).expect("url word");
        assert_eq!(url.text, "https://example.com/x");
        assert_eq!(url.kind, HoverKind::Url);
        assert_eq!((url.start_col, url.end_col), (5, 25));
        let path = word_target(&term, Point::new(Line(0), Column(37))).expect("path word");
        assert_eq!(path.text, "/tmp/foo.txt");
        assert_eq!(path.kind, HoverKind::Path);
        assert_eq!((path.start_col, path.end_col), (32, 43));
        assert!(word_target(&term, Point::new(Line(0), Column(1))).is_none());
    }

    #[test]
    fn hyperlink_span_covers_whole_link() {
        // OSC 8: "a " + linked "LINK-text" (cols 2..=10) + " z".
        let term = term_with("a \x1b]8;;https://example.com\x07LINK-text\x1b]8;;\x07 z");
        let target = hyperlink_span(&term, Point::new(Line(0), Column(4))).expect("hyperlink");
        assert_eq!(target.text, "https://example.com");
        assert_eq!((target.start_col, target.end_col), (2, 10));
        assert_eq!(target.kind, HoverKind::Url);
        assert!(hyperlink_span(&term, Point::new(Line(0), Column(0))).is_none());
    }
}
