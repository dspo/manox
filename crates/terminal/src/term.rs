//! `Terminal` Entity — the gpui state machine wrapping a rmux-core `Screen`.
//!
//! `Terminal` owns an `Arc<FairMutex<Screen>>` (the rmux-core grid/VT-engine),
//! a `Box<dyn PtySource>`, and a gpui task that drains the event channel:
//! `PtyOutput` is fed through `InputParser::parse` into the Screen under the
//! lock; side-effects (bell, passthrough, replies, title) are drained after
//! each parse batch and re-emitted via `EventEmitter<TerminalEvent>` for the
//! view layer.
//!
//! The Screen lock is taken only on the gpui side. The PTY reader/writer
//! threads never touch it — they move raw bytes over the channel.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, AppContext as _, AsyncApp, ClipboardItem, Context, Entity, EventEmitter, Task};
use parking_lot::Mutex;
use rmux_core::input::mode;
use rmux_core::input::InputParser;
use rmux_core::Screen;
use rmux_types::TerminalSize;

use crate::event::TerminalEvent;
use crate::pty_source::PtySource;
use crate::settings::BellMode;

/// The terminal state behind a `parking_lot::Mutex`.
type StateLock = Mutex<ScreenState>;

/// A screen + its parser, shared between the gpui task and the render path.
struct ScreenState {
    screen: Screen,
    parser: InputParser,
}

/// Mouse mode mask for checking whether the terminal captures the mouse.
const MOUSE_MODE_MASK: u32 = mode::MODE_MOUSE_STANDARD
    | mode::MODE_MOUSE_BUTTON
    | mode::MODE_MOUSE_ALL;

/// SGR mouse mode flag.
const MODE_MOUSE_SGR: u32 = mode::MODE_MOUSE_SGR;

/// Bracketed paste mode flag.
const MODE_BRACKETPASTE: u32 = mode::MODE_BRACKETPASTE;

pub struct Terminal {
    pub id: String,
    pub cwd: PathBuf,
    pub cols: usize,
    pub rows: usize,
    state: Arc<StateLock>,
    pty: Box<dyn PtySource>,
    pub child_exited: Option<i32>,
    pub title: Option<String>,
    /// Bell policy — the view reads this to decide whether to flash / beep.
    pub bell: BellMode,
    _task: Option<Task<()>>,
}

impl EventEmitter<TerminalEvent> for Terminal {}

impl Terminal {
    /// Create a Terminal running the given `pty` source in `cwd`. Bell policy
    /// comes from `[terminal]` in settings.toml; the PTY itself (shell, env)
    /// is supplied by the caller via the `PtySource`. The source is started
    /// here — its reader / waiter threads begin emitting events onto the
    /// channel the gpui task drains.
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
        let size = TerminalSize::new(cols as u16, rows as u16);
        let screen = Screen::new(size, settings.scrolling_history);
        let parser = InputParser::new();
        let state = Arc::new(Mutex::new(ScreenState { screen, parser }));
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
                        TerminalEvent::ClipboardStore(text) => {
                            let _ = this.update(cx, |_t: &mut Terminal, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            });
                        }
                        TerminalEvent::ClipboardReadReply(reply) => {
                            let _ = this.update(cx, |t: &mut Terminal, _cx| {
                                let _ = t.input(&reply);
                            });
                        }
                        TerminalEvent::PtyWrite(bytes) => {
                            let _ = this.update(cx, |t: &mut Terminal, _cx| {
                                let _ = t.input(&bytes);
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
                state,
                pty,
                child_exited: None,
                title: None,
                bell,
                _task: Some(task),
            }
        });
        Ok(entity)
    }

    /// Feed PTY output through the VT parser into the Screen, drain
    /// side-effects (bell, title, passthrough, replies), and nudge the view
    /// to repaint. Called only from the gpui task.
    fn write_pty_output(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let events: Vec<TerminalEvent> = {
            let ScreenState { screen, parser } = &mut *self.state.lock();
            parser.parse(bytes, screen);
            drain_side_effects(screen, parser)
        };
        for ev in events {
            match ev {
                TerminalEvent::Title(title) => {
                    self.title = title.clone();
                    cx.emit(TerminalEvent::Title(title));
                }
                TerminalEvent::Bell => {
                    cx.emit(TerminalEvent::Bell);
                }
                TerminalEvent::ClipboardStore(text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                TerminalEvent::ClipboardReadReply(reply) => {
                    let _ = self.pty.write(&reply);
                }
                TerminalEvent::PtyWrite(bytes) => {
                    let _ = self.pty.write(&bytes);
                }
                _ => {}
            }
        }
        cx.notify();
    }

    /// Send input bytes (keystrokes, paste) to the shell.
    pub fn input(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.pty.write(bytes)
    }

    /// Resize both the PTY and the Screen. No-op if unchanged.
    pub fn resize(&mut self, cols: usize, rows: usize, cx: &mut Context<Self>) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        let _ = self.pty.resize(cols as u16, rows as u16);
        let size = TerminalSize::new(cols as u16, rows as u16);
        let mut state = self.state.lock();
        state.screen.resize(size);
        drop(state);
        self.cols = cols;
        self.rows = rows;
        cx.notify();
    }

    /// Read-only access to the Screen for snapshot/render paths.
    pub fn with_screen<R>(&self, f: impl FnOnce(&Screen) -> R) -> R {
        let state = self.state.lock();
        f(&state.screen)
    }

    /// Current terminal mode flags — callers (key/mouse mapping) branch on
    /// mouse modes, bracketed paste, etc.
    pub fn mode(&self) -> u32 {
        self.with_screen(|s| s.mode())
    }

    /// Forward a mouse-wheel scroll to the PTY as xterm mouse reports, so a TUI
    /// app that captures the mouse (claude code / vim / htop) scrolls its own
    /// viewport instead of the (no-op, alt-screen) local scrollback.
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
        if mode & MOUSE_MODE_MASK == 0 {
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

    /// Paste text, wrapping in bracketed-paste markers when the mode is set.
    pub fn paste(&self, text: &str) -> std::io::Result<()> {
        let mode = self.mode();
        let bytes = if mode & MODE_BRACKETPASTE != 0 {
            format!("\x1b[200~{}\x1b[201~", text).into_bytes()
        } else {
            text.as_bytes().to_vec()
        };
        self.pty.write(&bytes)
    }

    /// The OSC 8 hyperlink URI at `(row, col)`, if any.
    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<String> {
        self.with_screen(|s| {
            // rmux-core's Screen numbers visible rows 0-based from the top.
            // Visit the row and check the cell's hyperlink id.
            let mut found: Option<String> = None;
            s.visit_visible_line_cells(row, col + 1, |cell| {
                if found.is_none() {
                    let link = cell.link();
                    if link != 0 && let Some(uri) = s.hyperlink_uri(link) {
                        found = Some(uri.to_owned());
                    }
                }
            });
            found
        })
    }
}

/// Drain side-effects from the Screen after a parse batch: bell count,
/// terminal passthrough events (which carry clipboard, DCS, sixel, etc.),
/// parser replies (DSR, DA, cursor-position reports), and title changes.
fn drain_side_effects(screen: &mut Screen, parser: &mut InputParser) -> Vec<TerminalEvent> {
    let mut events = Vec::new();

    // Bell.
    let bell_count = screen.take_bell_count();
    for _ in 0..bell_count {
        events.push(TerminalEvent::Bell);
    }

    // Title — check if it changed (Screen does not track "previous title").
    let title = screen.title().to_owned();
    if !title.is_empty() {
        events.push(TerminalEvent::Title(Some(title)));
    }

    // Terminal passthrough events. The `Clipboard` kind carries OSC 52
    // data; all passthrough payloads are tried through the clipboard
    // decoder — it returns None for non-clipboard payloads.
    for passthrough in screen.take_terminal_passthrough() {
        if let Some(text) = decode_clipboard_passthrough(passthrough.payload()) {
            events.push(TerminalEvent::ClipboardStore(text));
        }
    }

    // Parser replies: DSR cursor position reports, DA, XTVERSION, etc.
    // These bytes should be written back to the PTY so the application reads them.
    let replies = parser.take_replies();
    if !replies.is_empty() {
        events.push(TerminalEvent::PtyWrite(replies));
    }

    events
}

/// Decode an OSC 52 clipboard passthrough payload.
/// The payload format is the raw OSC 52 data after the `52;` prefix,
/// e.g. `c;<base64data>` for a clipboard set, or `c;?` for a query.
fn decode_clipboard_passthrough(data: &[u8]) -> Option<String> {
    // The passthrough payload includes the OSC 52 framing: `\x1b]52;<buf>\x07`
    // or `\x1b]52;<buf>\x1b\\`. We need to extract the base64 body.
    let data_str = std::str::from_utf8(data).ok()?;
    // Find the content after "52;" — the passthrough includes the full OSC.
    let content = if let Some(idx) = data_str.find("52;") {
        &data_str[idx + 3..]
    } else {
        // The passthrough may just be the payload without the OSC prefix.
        data_str
    };
    // Trim trailing ST/BEL terminators.
    let content = content
        .trim_end_matches('\x07')
        .trim_end_matches("\x1b\\");

    // Split on ';' to get the clipboard target and the base64 data.
    let parts: Vec<&str> = content.splitn(2, ';').collect();
    if parts.len() != 2 {
        return None;
    }
    // parts[0] is the clipboard target ('c' for clipboard, 'p' for primary).
    // parts[1] is '?' for a read query, or base64-encoded text for a write.
    if parts[1] == "?" {
        // Clipboard read query — manox does not support reading the clipboard
        // via OSC 52 (the application would need the system clipboard content).
        // This is a deliberate limitation; the read path would require a
        // round-trip through the gpui clipboard API.
        return None;
    }
    // Decode base64.
    let decoded = base64_decode(parts[1])?;
    String::from_utf8(decoded).ok()
}

/// Simple base64 decoder (avoids adding a base64 dependency just for this).
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let lookup = |c: u8| -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = [0u8; 4];
    let mut idx = 0;
    for &b in bytes {
        if b == b'=' || b == b'\n' || b == b'\r' {
            continue;
        }
        buf[idx] = lookup(b)?;
        idx += 1;
        if idx == 4 {
            out.push((buf[0] << 2) | (buf[1] >> 4));
            out.push((buf[1] << 4) | (buf[2] >> 2));
            out.push((buf[2] << 6) | buf[3]);
            idx = 0;
        }
    }
    if idx == 2 {
        out.push((buf[0] << 2) | (buf[1] >> 4));
    } else if idx == 3 {
        out.push((buf[0] << 2) | (buf[1] >> 4));
        out.push((buf[1] << 4) | (buf[2] >> 2));
    }
    Some(out)
}

/// Encode an xterm mouse report for `button` at visible grid `(row, col)`,
/// following the mode the TUI enabled:
/// - SGR (`\x1b[<`): `\x1b[<button;col+1;row+1M` (1-based, no +32 offset).
/// - Legacy / UTF8 (`\x1b[M`): `\x1b[M` + three payload bytes, each `32 +
///   value` (button code, 1-based column, 1-based row). Wheel button codes
///   (64/65 + modifiers) stay below 128, so the encoding is byte-identical
///   across legacy and UTF8 for the wheel case.
fn mouse_report_bytes(mode: u32, button: u8, row: usize, col: usize) -> Vec<u8> {
    if mode & MODE_MOUSE_SGR != 0 {
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

    #[test]
    fn sgr_wheel_report_is_one_based() {
        let b = mouse_report_bytes(MODE_MOUSE_SGR, 65, 2, 4);
        assert_eq!(b, b"\x1b[<65;5;3M");
    }

    #[test]
    fn legacy_wheel_report_adds_32_offset() {
        let b = mouse_report_bytes(0, 64, 0, 0);
        assert_eq!(b, b"\x1b[M\x60!!");
    }
}
