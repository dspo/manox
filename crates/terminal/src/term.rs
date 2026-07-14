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
use crate::settings::{BellMode, CursorShapeSetting, Osc52Access, TerminalSettings};

pub(crate) type ManoxTerm = Term<ManoxListener>;
pub(crate) type ManoxTermLock = FairMutex<ManoxTerm>;

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
fn build_config(settings: &TerminalSettings) -> Config {
    Config {
        scrolling_history: settings.scrolling_history,
        default_cursor_style: map_cursor(settings.cursor_shape),
        osc52: map_osc52(settings.osc52_access),
        ..Config::default()
    }
}

fn map_cursor(s: CursorShapeSetting) -> CursorStyle {
    let shape = match s {
        CursorShapeSetting::Block => CursorShape::Block,
        CursorShapeSetting::Underline => CursorShape::Underline,
        CursorShapeSetting::Beam => CursorShape::Beam,
    };
    CursorStyle {
        shape,
        blinking: false,
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
    pub child_exited: Option<i32>,
    pub title: Option<String>,
    /// Bell policy — the view reads this to decide whether to flash / beep.
    pub bell: BellMode,
    _task: Option<Task<()>>,
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
            Self {
                id,
                cwd,
                cols,
                rows,
                term,
                pty,
                output_processor: Processor::<StdSyncHandler>::new(),
                child_exited: None,
                title: None,
                bell,
                _task: Some(task),
            }
        });
        Ok(entity)
    }

    /// Feed PTY output through the vte processor into the Term, then nudge the
    /// view to repaint. Called only from the gpui task.
    fn write_pty_output(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let mut term = self.term.lock();
        for &b in bytes {
            self.output_processor.advance(&mut *term, b);
        }
        drop(term);
        cx.notify();
    }

    /// Send input bytes (keystrokes, paste) to the shell.
    pub fn input(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.pty.write(bytes)
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

    /// Selected text as a plain string, if a selection is active.
    pub fn selection_to_string(&self) -> Option<String> {
        self.with_term(|t| t.selection_to_string())
    }

    pub fn clear_selection(&self) {
        self.with_term_mut(|t| t.selection = None);
    }

    /// Begin a simple (char-granularity) selection at `(row, col)` in visible
    /// display coordinates. `row` 0 is the visible top line.
    pub fn start_selection(&self, row: usize, col: usize, cx: &mut Context<Self>) {
        self.with_term_mut(|t| {
            let point = self.display_point(t, row, col);
            t.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
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
}
