//! `Terminal` Entity — the gpui state machine wrapping an alacritty `Term`.
//!
//! `Terminal` owns an `Arc<FairMutex<ManoxTerm>>` (the alacritty grid/ANSI
//! engine), a `PtyHandle`, and a gpui task that drains the event channel:
//! `PtyOutput` is fed back into the Term under the lock; the rest are
//! re-emitted via `EventEmitter<TerminalEvent>` for the view layer.
//!
//! The Term lock is taken only on the gpui side. The PTY reader/writer
//! threads never touch it — they move raw bytes over the channel.

use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use anyhow::Result;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Task};

use crate::event::{ManoxListener, TerminalEvent};
use crate::pty;

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

pub struct Terminal {
    pub id: String,
    pub cwd: PathBuf,
    pub cols: usize,
    pub rows: usize,
    term: Arc<ManoxTermLock>,
    pty: pty::PtyHandle,
    output_processor: Processor<StdSyncHandler>,
    pub child_exited: Option<i32>,
    pub title: Option<String>,
    _task: Option<Task<()>>,
}

impl EventEmitter<TerminalEvent> for Terminal {}

impl Terminal {
    /// Create a Terminal running the user's default shell in `cwd`.
    pub fn new(
        id: String,
        cwd: PathBuf,
        cols: usize,
        rows: usize,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let (event_tx, event_rx) = async_channel::bounded::<TerminalEvent>(256);
        let listener = ManoxListener::new(event_tx.clone());
        let cfg = Config::default();
        let size = TermSize { cols, rows };
        let term = Arc::new(FairMutex::new(Term::new(cfg, &size, listener)));
        let pty = pty::spawn(&cwd, cols as u16, rows as u16, &[], event_tx.clone())?;

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

    /// Map a visible `(row, col)` to an alacritty grid `Point`. The grid's
    /// line 0 is the bottom-most screen line and grows negative into
    /// scrollback; `display_offset` shifts the visible window into history.
    fn display_point(&self, term: &ManoxTerm, row: usize, col: usize) -> Point {
        let offset = term.grid().display_offset() as i32;
        let line = row as i32 - (self.rows as i32 - 1) - offset;
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
        let pty =
            pty::spawn(&PathBuf::from("/tmp"), 80, 24, &[], event_tx.clone()).expect("spawn pty");

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
        // Drop kills the child and joins both threads.
        drop(pty);
    }
}
