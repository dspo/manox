//! `TerminalView` — the gpui `Render` wrapper around `TerminalElement`.
//!
//! Owns an `Entity<Terminal>`, renders the element full-bleed, and routes
//! keyboard/mouse/scroll input to the terminal. Key translation goes through
//! `mappings::keys::to_esc_str`; mouse left-drag does char-granularity
//! selection + copy-to-clipboard on release; the scroll wheel scrolls the
//! scrollback. Mouse-reporting modes (vim/htop) forward to the PTY instead
//! of local selection (stage 5 wires the encode call).

use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, FocusHandle, Font, FontFeatures, FontStyle,
    FontWeight, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollDelta,
    ScrollWheelEvent, Styled, Window, black, div, px,
};
use terminal::Terminal;
use terminal::alacritty_terminal::term::TermMode;
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
        let mode = self.terminal.read_with(cx, |t, _| t.mode());
        if let Some(s) = keys::to_esc_str(&ev.keystroke, mode) {
            let _ = self.terminal.update(cx, |t, _cx| t.input(s.as_bytes()));
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
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
                theme: TerminalTheme::default(),
                font: self.font.clone(),
                font_size: self.font_size,
                line_height: self.line_height,
            })
    }
}
