//! `TerminalView` — the gpui `Render` wrapper around `TerminalElement`.
//!
//! Owns an `Entity<Terminal>`, renders the element full-bleed, and routes
//! keyboard/mouse/scroll input to the terminal. Key translation goes through
//! `mappings::keys::to_esc_str`; mouse left-drag does char-granularity
//! selection + copy-to-clipboard on release; the scroll wheel scrolls the
//! scrollback. Mouse-reporting modes (vim/htop) forward to the PTY instead
//! of local selection. IME composition (CJK) is handled via a gpui
//! `InputHandler` registered by the element each frame; committed text is
//! written to the PTY and the in-flight marked text is painted inline at the
//! cursor.

use std::ops::Range;

use gpui::{
    App, AppContext, Bounds, ClipboardItem, Context, Entity, FocusHandle, Font, FontFeatures,
    FontStyle, FontWeight, InputHandler, InteractiveElement, IntoElement, KeyDownEvent, Keystroke,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    Render, ScrollDelta, ScrollWheelEvent, SharedString, Styled, UTF16Selection, Window, black,
    div, px,
};
use terminal::Terminal;
use terminal::alacritty_terminal::term::TermMode;
use terminal::alacritty_terminal::vi_mode::ViMotion;
use terminal::mappings::keys;

use crate::element::TerminalElement;
use crate::theme::TerminalTheme;

/// A view that hosts one terminal session. Created by the workspace when the
/// user opens a terminal tab.
pub struct TerminalView {
    terminal: Entity<Terminal>,
    focus_handle: FocusHandle,
    font: Font,
    font_size: Pixels,
    line_height: f32,
    /// True while the left mouse button is held after a press in the element,
    /// so `on_mouse_move` extends the selection.
    selecting: bool,
    /// In-flight IME marked (preedit) text, painted at the cursor by the
    /// element. Empty when no composition is active.
    marked_text: String,
}

impl TerminalView {
    pub fn new(terminal: Entity<Terminal>, cx: &mut App) -> Entity<Self> {
        let terminal_for_view = terminal.clone();
        let view = cx.new(move |cx| Self {
            terminal: terminal_for_view,
            focus_handle: cx.focus_handle(),
            font: Font {
                family: "Menlo".into(),
                features: FontFeatures::default(),
                fallbacks: None,
                weight: FontWeight::default(),
                style: FontStyle::Normal,
            },
            font_size: px(14.),
            line_height: 1.2,
            selecting: false,
            marked_text: String::new(),
        });
        cx.subscribe(&terminal, {
            let view = view.clone();
            move |_t, _ev: &terminal::event::TerminalEvent, cx| {
                view.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
        view
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;

        // Toggle the terminal's built-in vi mode (alacritty's, not `vim`)
        // on ctrl+shift+v.
        if k.modifiers.control && k.modifiers.shift && k.key == "v" {
            self.terminal.update(cx, |t, cx| t.toggle_vi_mode(cx));
            return;
        }

        let mode = self.terminal.read_with(cx, |t, _| t.mode());

        // In vi mode, motion keys move the vi cursor and are NOT forwarded
        // to the PTY; unmapped keys are swallowed.
        if mode.contains(TermMode::VI) {
            if let Some(motion) = vi_motion_for(k) {
                self.terminal.update(cx, |t, cx| t.vi_motion(motion, cx));
            }
            return;
        }

        if let Some(s) = keys::to_esc_str(k, mode) {
            let _ = self.terminal.update(cx, |t, _cx| t.input(s.as_bytes()));
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // cmd/ctrl+click opens an OSC 8 hyperlink under the cursor.
        if ev.modifiers.platform || ev.modifiers.control {
            let (row, col) = self.px_to_grid(ev.position, window);
            if let Some(url) = self.terminal.read_with(cx, |t, _| t.hyperlink_at(row, col)) {
                let _ = std::process::Command::new("open").arg(url).spawn();
                return;
            }
        }
        // Mouse-reporting modes: the TUI app owns the mouse; defer to the
        // PTY report path (stage 5) instead of starting a local selection.
        let mode = self.terminal.read_with(cx, |t, _| t.mode());
        if mode.intersects(TermMode::MOUSE_MODE) || ev.button != MouseButton::Left {
            return;
        }
        let (row, col) = self.px_to_grid(ev.position, window);
        self.terminal
            .update(cx, |t, cx| t.start_selection(row, col, cx));
        self.selecting = true;
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let (row, col) = self.px_to_grid(ev.position, window);
        self.terminal
            .update(cx, |t, cx| t.update_selection(row, col, cx));
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if ev.button != MouseButton::Left || !self.selecting {
            return;
        }
        self.selecting = false;
        if let Some(text) = self.terminal.read_with(cx, |t, _| t.selection_to_string()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn on_scroll_wheel(
        &mut self,
        ev: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Negative = scroll up into scrollback history.
        let lines = match ev.delta {
            ScrollDelta::Pixels(p) => -(f32::from(p.y) / 20.) as i32,
            ScrollDelta::Lines(l) => -(l.y as i32),
        };
        if lines != 0 {
            self.terminal.update(cx, |t, cx| t.scroll(lines, cx));
        }
    }

    /// Map an element-relative pixel position to `(row, col)` grid coords by
    /// measuring the monospace cell width from the same font the element
    /// paints with.
    fn px_to_grid(&self, pos: Point<Pixels>, window: &Window) -> (usize, usize) {
        let cell_w = self.cell_width(window);
        let line_h = px(f32::from(self.font_size) * self.line_height);
        let col = (f32::from(pos.x) / f32::from(cell_w)).max(0.).floor() as usize;
        let row = (f32::from(pos.y) / f32::from(line_h)).max(0.).floor() as usize;
        (row, col)
    }

    fn cell_width(&self, window: &Window) -> Pixels {
        let probe = gpui::TextRun {
            len: 1,
            font: self.font.clone(),
            color: black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window.text_system().shape_line(
            "m".into(),
            self.font_size,
            std::slice::from_ref(&probe),
            None,
        );
        shaped.width().max(px(1.))
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_1()
            .w_full()
            .h_full()
            .bg(black())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .child(TerminalElement {
                terminal: self.terminal.clone(),
                view: cx.entity(),
                focus_handle: self.focus_handle.clone(),
                theme: TerminalTheme::default(),
                font: self.font.clone(),
                font_size: self.font_size,
                line_height: self.line_height,
                marked_text: SharedString::from(self.marked_text.clone()),
            })
    }
}

impl TerminalView {
    fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.marked_text = text;
        cx.notify();
    }

    fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if !self.marked_text.is_empty() {
            self.marked_text.clear();
            cx.notify();
        }
    }

    /// Commit finalized IME / direct text input to the PTY.
    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        let _ = self.terminal.update(cx, |t, _| t.input(text.as_bytes()));
    }
}

/// gpui `InputHandler` driving IME composition for a focused terminal view.
///
/// `prefers_ime_for_printable_keys` is `true` so that when a non-ASCII input
/// source (CJK) is active, keystrokes reach the IME for composition instead of
/// being dispatched as raw key events; with an ASCII source, raw keys still
/// flow through `on_key_down`. Committed text is written to the PTY as plain
/// input (not bracketed paste — IME commits are normal keyboard input).
pub struct TerminalInputHandler {
    /// The view owning the terminal and marked-text state.
    pub view: Entity<TerminalView>,
    /// Cursor pixel bounds from the latest paint, used to place the IME
    /// candidate window.
    pub cursor_bounds: Option<Bounds<Pixels>>,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        // Signal "input enabled, caret at 0..0" so the platform engages IME.
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.view.read_with(cx, |v, _| {
            if v.marked_text.is_empty() {
                None
            } else {
                Some(0..v.marked_text.chars().count())
            }
        })
    }

    fn text_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.view.update(cx, |view, view_cx| {
            view.clear_marked_text(view_cx);
            view.commit_text(text, view_cx);
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.view.update(cx, |view, view_cx| {
            view.set_marked_text(new_text.to_string(), view_cx)
        });
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.view
            .update(cx, |view, view_cx| view.clear_marked_text(view_cx));
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }

    fn prefers_ime_for_printable_keys(&mut self, _window: &mut Window, _cx: &mut App) -> bool {
        true
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }
}

/// Map a vi-mode keystroke to an alacritty `ViMotion`. Returns `None` for
/// keys without a mapping (the caller swallows them in vi mode).
fn vi_motion_for(k: &Keystroke) -> Option<ViMotion> {
    if k.modifiers.control || k.modifiers.alt {
        return None;
    }
    let shift = k.modifiers.shift;
    Some(match k.key.as_ref() {
        "h" => ViMotion::Left,
        "j" => ViMotion::Down,
        "k" => ViMotion::Up,
        "l" => ViMotion::Right,
        "0" => ViMotion::First,
        "4" if shift => ViMotion::Last, // $ = shift+4
        "w" => ViMotion::WordRight,
        "b" => ViMotion::WordLeft,
        "e" => ViMotion::WordRightEnd,
        "g" if shift => ViMotion::Low, // G → bottom
        _ => return None,
    })
}
