//! `Terminal` state machine wrapping an alacritty `Term`, fronted by a
//! gpui-free `TerminalHandle`.
//!
//! `Terminal` owns an `Arc<FairMutex<ManoxTerm>>` (the alacritty grid/ANSI
//! engine) and a mutex-guarded `Box<dyn PtySource>`. Runtime pumps drain the
//! event channel: `PtyOutput` is fed back into the Term under the lock; the rest are
//! translated on the handle and broadcast to channel subscribers.
//!
//! The Term lock is taken only on the terminal side. The PTY reader/writer
//! threads never touch it — they move raw bytes over the channel.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use hyperlinks::{
    OverlaySpan, PathOptions, UrlKind, default_path_options, detect_paths, detect_urls, trim_url,
};

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vi_mode::ViMotion;
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle, Processor, StdSyncHandler};
use anyhow::Result;

use crate::mappings::keys::Modifiers;

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

impl HoverTarget {
    /// Map a library link span onto a grid hover target. `row` is stamped by
    /// [`Terminal::hover_target`]; the column span is the span's extent on the
    /// hovered display row.
    pub fn from_overlay(span: &OverlaySpan, row: usize, start_col: usize, end_col: usize) -> Self {
        HoverTarget {
            text: span.href.clone(),
            row,
            start_col,
            end_col,
            kind: match span.kind {
                UrlKind::Url => HoverKind::Url,
                UrlKind::Path => HoverKind::Path,
            },
        }
    }
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
/// `Config.osc52`, so the event pump only sees allowed clipboard requests.
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
    /// Whether block characters render as sub-grid rects (settings-derived;
    /// off falls back to font shaping).
    block_char_render: bool,
    term: Arc<ManoxTermLock>,
    /// The PTY source is `Send` but not `Sync` (portable-pty's boxed
    /// master/reader), so it sits behind a mutex to keep `Terminal: Sync` —
    /// the handle's `RwLock` (and the runtime pumps that share it) need it.
    pty: parking_lot::Mutex<Box<dyn PtySource>>,
    output_processor: Processor<StdSyncHandler>,
    /// Byte tap observing the PTY stream for the readiness marker and OSC 7
    /// cwd reports, parallel to the vte processor.
    tap: OscTap,
    readiness: ReadinessTracker,
    pub child_exited: Option<i32>,
    pub title: Option<String>,
    /// Bell policy — the view reads this to decide whether to flash / beep.
    pub bell: BellMode,
    /// Events buffered under the state lock; [`TerminalHandle::with_mut`]
    /// drains and broadcasts them once the mutation closure returns.
    pending_events: Vec<TerminalEvent>,
}

/// The gpui-free handle to a terminal. Cheap to clone (`Arc`); state lives
/// behind a lock and events broadcast to channel subscribers. This is the
/// unit the view layer and the store hold in place of an `Entity<Terminal>`.
#[derive(Clone)]
pub struct TerminalHandle(Arc<TerminalCore>);

pub struct TerminalCore {
    /// Read-mostly state: the view reads the grid per render while the PTY
    /// pump writes, so reads share the `RwLock` and only mutations take the
    /// exclusive write lock.
    state: parking_lot::RwLock<Terminal>,
    /// Event subscribers. Carries `Arc<TerminalEvent>` because
    /// `TerminalEvent` holds non-`Clone` alacritty callbacks (`ColorRequest`,
    /// `ClipboardLoad`); the `Arc` lets one event fan out to every subscriber.
    subscribers: parking_lot::Mutex<Vec<async_channel::Sender<Arc<TerminalEvent>>>>,
}

impl TerminalHandle {
    /// Wrap a freshly built [`Terminal`].
    pub fn new(terminal: Terminal) -> Self {
        Self(Arc::new(TerminalCore {
            state: parking_lot::RwLock::new(terminal),
            subscribers: parking_lot::Mutex::new(Vec::new()),
        }))
    }

    /// Downgrade to a weak reference so a long-lived pump (or a registry
    /// index) never by itself keeps the terminal alive; the strong reference
    /// sits with the view / store holder.
    pub fn downgrade(&self) -> std::sync::Weak<TerminalCore> {
        Arc::downgrade(&self.0)
    }

    /// Re-upgrade a weak reference, if the terminal is still alive.
    pub fn upgrade(weak: &std::sync::Weak<TerminalCore>) -> Option<Self> {
        weak.upgrade().map(Self)
    }

    /// Subscribe to this terminal's event stream.
    pub fn subscribe(&self) -> async_channel::Receiver<Arc<TerminalEvent>> {
        let (tx, rx) = async_channel::unbounded();
        self.0.subscribers.lock().push(tx);
        rx
    }

    /// Shared-read the state.
    pub fn read<R>(&self, f: impl FnOnce(&Terminal) -> R) -> R {
        let state = self.0.state.read();
        f(&state)
    }

    /// Mutate under the write lock, then broadcast the buffered events.
    /// Three-phase: lock -> mutate (collecting `pending_events`) -> unlock ->
    /// emit. The closure must never await.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut Terminal) -> R) -> R {
        let (r, events) = {
            let mut state = self.0.state.write();
            let r = f(&mut state);
            let events = std::mem::take(&mut state.pending_events);
            (r, events)
        };
        self.broadcast(events);
        r
    }

    fn broadcast(&self, events: Vec<TerminalEvent>) {
        if events.is_empty() {
            return;
        }
        let mut subs = self.0.subscribers.lock();
        // Drop subscribers whose receiver is gone (view unmount); otherwise
        // the list grows without bound on a long-lived terminal.
        subs.retain(|tx| !tx.is_closed());
        if subs.is_empty() {
            return;
        }
        for ev in events {
            let ev = Arc::new(ev);
            for tx in subs.iter() {
                let _ = tx.try_send(ev.clone());
            }
        }
    }
}

impl Terminal {
    /// Create a Terminal running the given `pty` source in `cwd`. Font,
    /// scrollback, cursor, bell, and OSC 52 policy come from `[terminal]` in
    /// settings.toml; the PTY itself (shell, env) is supplied by the caller via
    /// the `PtySource`. The source is started here — its reader / waiter
    /// threads begin emitting events onto the channel the runtime pumps drain.
    pub fn spawn(
        id: String,
        cwd: PathBuf,
        cols: usize,
        rows: usize,
        mut pty: Box<dyn PtySource>,
    ) -> Result<TerminalHandle> {
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
        // threads before the runtime pumps start draining the channel.
        pty.start(event_tx.clone());

        let terminal = Terminal {
            id,
            cwd,
            cols,
            rows,
            block_char_render: settings.block_char_render,
            term,
            pty: parking_lot::Mutex::new(pty),
            output_processor: Processor::<StdSyncHandler>::new(),
            tap: OscTap::new(ready_nonce),
            readiness: ReadinessTracker::new(readiness_mode, Instant::now()),
            child_exited: None,
            title: None,
            bell,
            pending_events: Vec::new(),
        };
        let handle = TerminalHandle::new(terminal);
        handle.start_event_pump(event_rx);
        handle.start_readiness_pump();
        Ok(handle)
    }
}

impl TerminalHandle {
    /// Drain the PTY / listener event channel on the registered runtime,
    /// dispatching each event through [`Self::on_event`]. The pump holds only
    /// a weak reference: once every strong `TerminalHandle` is dropped the
    /// upgrade fails and the pump exits, so dead terminals never leak pumps.
    fn start_event_pump(&self, rx: async_channel::Receiver<TerminalEvent>) {
        let weak = self.downgrade();
        crate::runtime::handle().spawn(async move {
            while let Ok(ev) = rx.recv().await {
                let Some(handle) = TerminalHandle::upgrade(&weak) else {
                    return;
                };
                handle.on_event(ev);
            }
        });
    }

    /// Poll readiness on a fixed 100ms cadence — fallback / heuristic
    /// transitions need a clock even when no output arrives; marker hits
    /// transition in `write_pty_output`. Exits once the terminal is ready or
    /// dropped.
    fn start_readiness_pump(&self) {
        let weak = self.downgrade();
        crate::runtime::handle().spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let Some(handle) = TerminalHandle::upgrade(&weak) else {
                    return;
                };
                let ready = handle.with_mut(|t| {
                    if t.readiness.poll(Instant::now()) {
                        t.emit_ready();
                    }
                    t.readiness.is_ready()
                });
                if ready {
                    return;
                }
            }
        });
    }

    /// Translate one channel event into state and subscriber events. The
    /// arms mirror the former gpui pump: state mutations run under `with_mut`
    /// (buffering into `pending_events`, broadcast on exit); the clipboard
    /// arms go through the capability seam and never take the state lock.
    fn on_event(&self, ev: TerminalEvent) {
        match ev {
            TerminalEvent::PtyOutput(bytes) => self.with_mut(|t| t.write_pty_output(&bytes)),
            TerminalEvent::ChildExit(code) => self.with_mut(|t| {
                t.child_exited = Some(code);
                t.pending_events.push(TerminalEvent::ChildExit(code));
            }),
            TerminalEvent::Title(title) => self.with_mut(|t| {
                t.title = title.clone();
                t.pending_events.push(TerminalEvent::Title(title));
            }),
            // OSC 52 write: store text on the system clipboard. Fail closed:
            // with no provider (or a refusing one) the copy is dropped.
            TerminalEvent::ClipboardStore(text) => match manox_agent::capability::provider() {
                Some(p) => {
                    if let Err(e) = p.clipboard_write(text) {
                        tracing::warn!(error = %e, "terminal clipboard write failed");
                    }
                }
                None => {
                    tracing::warn!("no clipboard capability provider; dropping OSC 52 copy");
                }
            },
            // OSC 52 read: load the clipboard, let the TUI's callback format
            // its response, write that back to the PTY so the application can
            // read it. The read is a frontend round-trip, so it runs on its
            // own task rather than blocking this pump. Fail closed: without a
            // provider (or on error) the response is built from empty text,
            // so no clipboard content is ever injected.
            TerminalEvent::ClipboardLoad(cb) => {
                let Some(p) = manox_agent::capability::provider() else {
                    tracing::warn!("no clipboard capability provider; OSC 52 paste returns empty");
                    let response = cb("");
                    let _ = self.read(|t| t.input(response.as_bytes()));
                    return;
                };
                let handle = self.clone();
                crate::runtime::handle().spawn(async move {
                    let text = match p.clipboard_read().await {
                        Ok(Some(s)) => s,
                        Ok(None) => String::new(),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "terminal clipboard read failed; OSC 52 paste returns empty"
                            );
                            String::new()
                        }
                    };
                    let response = cb(&text);
                    let _ = handle.read(|t| t.input(response.as_bytes()));
                });
            }
            // Bytes the TUI emitted via the terminal (rare; e.g. some DCS
            // responses). Forward to the PTY verbatim.
            TerminalEvent::PtyWrite(text) => {
                let _ = self.read(|t| t.input(text.as_bytes()));
            }
            // Everything else (Wakeup, Bell, ColorRequest, ...) is purely
            // for the view: re-emit it on the subscriber channel.
            other => self.with_mut(|t| t.pending_events.push(other)),
        }
    }
}

impl Terminal {
    /// Feed PTY output through the vte processor into the Term. Called only
    /// from the event pump, under the handle's write lock; the view repaints
    /// off the broadcast `Wakeup` / output-adjacent events.
    fn write_pty_output(&mut self, bytes: &[u8]) {
        for ev in self.tap.feed(bytes) {
            match ev {
                TapEvent::ReadyMarker => {
                    if self.readiness.on_marker() {
                        self.emit_ready();
                    }
                }
                TapEvent::Cwd(path) => {
                    if self.cwd != path {
                        self.cwd = path.clone();
                        self.pending_events.push(TerminalEvent::CwdChanged(path));
                    }
                }
            }
        }
        self.readiness.on_output(Instant::now());
        let mut term = self.term.lock();
        for &b in bytes {
            self.output_processor.advance(&mut *term, b);
        }
    }

    /// Whether the shell finished init and accepts input — marker tap, quiet
    /// window, or fallback timeout, whichever came first. Drives the view's
    /// starting indicator.
    pub fn is_ready(&self) -> bool {
        self.readiness.is_ready()
    }

    /// Buffer the readiness transition; `with_mut` broadcasts it on exit.
    /// Callers guard on the tracker, so this fires at most once per terminal.
    fn emit_ready(&mut self) {
        self.pending_events.push(TerminalEvent::Ready);
    }

    /// Send input bytes (keystrokes, paste) to the shell.
    pub fn input(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.pty.lock().write(bytes)
    }

    /// Whether block characters (`▀▌▓…`) paint as sub-grid rects rather than
    /// font-shaped glyphs. Mirrors the `[terminal] block_char_render` setting.
    pub fn block_char_render(&self) -> bool {
        self.block_char_render
    }

    /// Name of the process owning the foreground process group, when it is
    /// not the shell itself. The view polls this on a slow timer for its
    /// foreground-process chip; `None` hides the chip (idle prompt, or the
    /// source cannot tell).
    pub fn foreground_process_name(&self) -> Option<String> {
        self.pty.lock().foreground_process_name()
    }

    /// Resize both the PTY and the Term. No-op if unchanged.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        let _ = self.pty.lock().resize(cols as u16, rows as u16);
        let mut term = self.term.lock();
        term.resize(TermSize { cols, rows });
        drop(term);
        self.cols = cols;
        self.rows = rows;
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
    pub fn scroll(&self, delta: i32) {
        self.with_term_mut(|t| t.scroll_display(Scroll::Delta(delta)));
    }

    /// Scroll the display to the offset implied by a scrollbar drag at
    /// `fraction` down the track (0 = top / oldest scrollback, 1 = bottom /
    /// live edge). alacritty clamps the resulting offset to the history.
    pub fn scroll_to_fraction(&self, fraction: f32) {
        self.with_term_mut(|t| {
            let history = t.grid().history_size();
            let target = ((1. - fraction.clamp(0., 1.)) * history as f32).round() as i32;
            let current = t.grid().display_offset() as i32;
            t.scroll_display(Scroll::Delta(target - current));
        });
    }

    /// Forward a mouse-wheel scroll to the PTY as xterm mouse reports, so a TUI
    /// app that captures the mouse (claude code / vim / htop) scrolls its own
    /// viewport instead of the (no-op, alt-screen) local scrollback. `delta_lines`
    /// is signed (negative = wheel up, positive = wheel down); one report per
    /// line, capped at a small burst so a single fling does not flood the PTY.
    /// `row`/`col` are the visible grid coords under the cursor. No-op when no
    /// mouse mode is active — callers should fall back to [`Self::scroll`].
    pub fn mouse_wheel(&self, row: usize, col: usize, delta_lines: i32, modifiers: &Modifiers) {
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
        let pty = self.pty.lock();
        for _ in 0..count {
            let _ = pty.write(&report);
        }
    }

    /// xterm alternateScroll: with the alt screen active but no mouse capture
    /// (less, git log), wheel deltas become arrow-key presses so the program
    /// scrolls its own content. No-op unless [`alternate_scroll_active`]
    /// holds. `delta_lines` shares the wheel sign convention (negative
    /// = up); capped per event like mouse reports.
    pub fn alternate_scroll(&self, delta_lines: i32) {
        if delta_lines == 0 {
            return;
        }
        let mode = self.mode();
        if !alternate_scroll_active(mode) {
            return;
        }
        let _ = self
            .pty
            .lock()
            .write(&alternate_scroll_bytes(mode, delta_lines));
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
    pub fn start_selection(&self, ty: SelectionType, row: usize, col: usize) {
        self.with_term_mut(|t| {
            let point = self.display_point(t, row, col);
            t.selection = Some(Selection::new(ty, point, Side::Left));
        });
    }

    /// Extend the existing selection to `(row, col)`. No-op if no selection.
    pub fn update_selection(&self, row: usize, col: usize) {
        self.with_term_mut(|t| {
            if t.selection.is_none() {
                return;
            }
            let point = self.display_point(t, row, col);
            if let Some(sel) = t.selection.as_mut() {
                sel.update(point, Side::Right);
            }
        });
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
        self.pty.lock().write(&bytes)
    }

    /// Toggle the terminal's built-in vi mode (alacritty's, not the `vim`
    /// process) — used for keyboard-driven selection/scrollback navigation.
    pub fn toggle_vi_mode(&self) {
        self.with_term_mut(|t| t.toggle_vi_mode());
    }

    /// Apply a vi motion. Only meaningful while vi mode is on.
    pub fn vi_motion(&self, motion: ViMotion) {
        self.with_term_mut(|t| t.vi_motion(motion));
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
    /// first, else the semantic word when it classifies as a URL or a path
    /// (wrapped across display lines the word is merged). `None` outside the
    /// visible grid or on plain text.
    pub fn hover_target(&self, row: usize, col: usize) -> Option<HoverTarget> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.with_term(|t| {
            let point = self.display_point(t, row, col);
            hyperlink_span(t, point).or_else(|| word_target(t, point, Some(&self.cwd)))
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

/// Whether the wheel degenerates into arrow-key presses (xterm
/// alternateScroll): the alt screen is active, DECSET 1007 has not turned
/// the mode off, and the program has not captured the mouse. alacritty
/// enables ALTERNATE_SCROLL by default, so the alt-screen half of the
/// condition is the real gate — on the normal screen the wheel must stay
/// on the local scrollback.
pub fn alternate_scroll_active(mode: TermMode) -> bool {
    mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
        && !mode.intersects(TermMode::MOUSE_MODE)
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

/// The semantic word at `point` when it looks like a URL or a path. The word
/// may span display lines (soft-wrapped links): the fragments are merged and
/// the whole is classified, so a wrapped URL stays one hoverable target.
fn word_target(term: &ManoxTerm, point: Point, cwd: Option<&Path>) -> Option<HoverTarget> {
    let grid = term.grid();
    let start = term.semantic_search_left(point);
    let end = term.semantic_search_right(point);
    let mut text = String::new();
    for line in start.line.0..=end.line.0 {
        let row = &grid[Line(line)];
        let from = if line == start.line.0 {
            start.column.0
        } else {
            0
        };
        let to = if line == end.line.0 {
            end.column.0
        } else {
            grid.columns() - 1
        };
        for c in from..=to {
            text.push(row[Column(c)].c);
        }
    }
    // Trailing padding / wide-char spacers are not part of the word, then the
    // URL boundary trim strips trailing punctuation and closing brackets.
    let text = text.trim_end().to_owned();
    let trimmed = trim_url(&text).to_owned();
    let span = classify(&trimmed, cwd)?;
    let (start_col, end_col) = if start.line == end.line {
        (
            start.column.0,
            start.column.0 + trimmed.len().saturating_sub(1),
        )
    } else {
        // The underline covers the hovered row's fragment; trims only shorten
        // the last line's fragment.
        let from = if start.line.0 == point.line.0 {
            start.column.0
        } else {
            0
        };
        let to = if end.line.0 == point.line.0 {
            end.column
                .0
                .saturating_sub(text.len().saturating_sub(trimmed.len()))
        } else {
            grid.columns() - 1
        };
        (from, to)
    };
    Some(HoverTarget::from_overlay(
        &span, // Stamped with the display row by `hover_target`.
        0, start_col, end_col,
    ))
}

/// Classify a semantic word via the shared link library: a URL span first,
/// else a path span (`/`-containing with extension / line anchor / cwd
/// existence).
fn classify(text: &str, cwd: Option<&Path>) -> Option<OverlaySpan> {
    if let Some(span) = detect_urls(text).into_iter().next() {
        return Some(span);
    }
    let opts = PathOptions {
        cwd: cwd.map(PathBuf::from),
        ..default_path_options()
    };
    detect_paths(text, &opts).into_iter().next()
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

    /// End-to-end PTY+Term loop without the `Terminal` state machine: spawn
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

    /// alacritty turns ALTERNATE_SCROLL on by default, so a normal-screen
    /// program (inline TUI, shell) satisfies neither half of the gate by
    /// itself: no arrows without the alt screen, no arrows under mouse
    /// capture, arrows only on the alt screen with the mode left on.
    #[test]
    fn alternate_scroll_active_requires_alt_screen() {
        let base = TermMode::default();
        assert!(base.contains(TermMode::ALTERNATE_SCROLL));
        assert!(!alternate_scroll_active(base));
        assert!(alternate_scroll_active(base | TermMode::ALT_SCREEN));
        assert!(!alternate_scroll_active(
            base | TermMode::ALT_SCREEN | TermMode::MOUSE_REPORT_CLICK
        ));
        assert!(!alternate_scroll_active(
            (base | TermMode::ALT_SCREEN) - TermMode::ALTERNATE_SCROLL
        ));
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
        term_with_size(text, 80, 24)
    }

    /// A standalone Term with a custom grid size, for wrap behavior tests.
    fn term_with_size(text: &str, cols: usize, rows: usize) -> ManoxTerm {
        let (event_tx, _rx) = async_channel::bounded::<TerminalEvent>(256);
        let listener = ManoxListener::new(event_tx);
        let cfg = build_config(&TerminalSettings::default());
        let size = TermSize { cols, rows };
        let mut term = Term::new(cfg, &size, listener);
        let mut processor = Processor::<StdSyncHandler>::new();
        for &b in text.as_bytes() {
            processor.advance(&mut term, b);
        }
        term
    }

    #[test]
    fn classify_rules() {
        assert_eq!(
            classify("https://a.b/c", None),
            Some(OverlaySpan {
                href: "https://a.b/c".into(),
                range: 0..13,
                kind: UrlKind::Url,
            })
        );
        assert_eq!(
            classify("http://a.b", None).map(|s| s.kind),
            Some(UrlKind::Url)
        );
        assert_eq!(
            classify("mailto:x@y.z", None).map(|s| s.kind),
            Some(UrlKind::Url)
        );
        assert_eq!(
            classify("src/main.rs", None).map(|s| s.kind),
            Some(UrlKind::Path)
        );
        // Extension-less, non-existent paths need a cwd to classify.
        assert_eq!(classify("/tmp/x", None), None);
        assert_eq!(classify("hello", None), None);
    }

    #[test]
    fn word_target_classifies_url_and_path() {
        // "open https://example.com/x then /tmp/foo.txt": url cols 5..=25,
        // path cols 32..=43 — `:` stays inside the word per build_config's
        // separator set.
        let term = term_with("open https://example.com/x then /tmp/foo.txt\r\n");
        let url = word_target(&term, Point::new(Line(0), Column(8)), None).expect("url word");
        assert_eq!(url.text, "https://example.com/x");
        assert_eq!(url.kind, HoverKind::Url);
        assert_eq!((url.start_col, url.end_col), (5, 25));
        let path = word_target(&term, Point::new(Line(0), Column(37)), None).expect("path word");
        assert_eq!(path.text, "/tmp/foo.txt");
        assert_eq!(path.kind, HoverKind::Path);
        assert_eq!((path.start_col, path.end_col), (32, 43));
        assert!(word_target(&term, Point::new(Line(0), Column(1)), None).is_none());
    }

    #[test]
    fn word_target_merges_wrapped_url_across_lines() {
        // At 20 cols the URL "https://example.com/verylong/path" wraps onto a
        // second display line; hovering the first fragment must still yield
        // the whole URL as one target.
        let term = term_with_size("go https://example.com/verylong/path here\r\n", 20, 4);
        let target = word_target(&term, Point::new(Line(0), Column(8)), None).expect("wrapped url");
        assert_eq!(target.text, "https://example.com/verylong/path");
        assert_eq!(target.kind, HoverKind::Url);
        // The underline covers the hovered row's fragment (cols 3..=19).
        assert_eq!((target.start_col, target.end_col), (3, 19));
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
