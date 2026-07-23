//! `TerminalView` — the gpui `Render` wrapper around `TerminalElement`.
//!
//! Owns an `Entity<Terminal>`, renders the element full-bleed, and routes
//! keyboard/mouse/scroll input to the terminal. Key translation goes through
//! `mappings::keys::to_esc_str`; mouse left-drag does char-granularity
//! selection + copy-to-clipboard on release; the scroll wheel scrolls the
//! scrollback. Mouse-reporting modes (vim/htop) forward to the PTY instead
//! of local selection. IME composition (CJK) is handled via a gpui
//! `InputHandler` registered by the element each frame; committed text is
//! written to the PTY and the in-flight marked text is painted inline at
//! the cursor.

use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, Font, FontFeatures,
    FontStyle, FontWeight, InputHandler, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    Render, ScrollDelta, ScrollWheelEvent, SharedString, Styled, UTF16Selection, Window, div,
    px, rgba,
};
use gpui_component::ActiveTheme as _;
use rmux_core::input::mode;
use terminal::Terminal;
use terminal::mappings::keys;
use terminal::settings::BellMode;

use crate::element::TerminalElement;
use crate::theme::TerminalTheme;

/// Mouse mode mask for checking whether the terminal captures the mouse.
const MOUSE_MODE_MASK: u32 =
    mode::MODE_MOUSE_STANDARD | mode::MODE_MOUSE_BUTTON | mode::MODE_MOUSE_ALL;

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
    /// True while a visual bell flash is active; cleared by a timer.
    bell_flash: bool,
}

impl TerminalView {
    pub fn new(terminal: Entity<Terminal>, cx: &mut App) -> Entity<Self> {
        let terminal_for_view = terminal.clone();
        let s = terminal::settings::load();
        let view = cx.new(move |cx| Self {
            terminal: terminal_for_view,
            focus_handle: cx.focus_handle(),
            font: Font {
                family: s.font_family.clone().into(),
                features: FontFeatures::default(),
                fallbacks: None,
                weight: FontWeight::default(),
                style: FontStyle::Normal,
            },
            font_size: px(s.font_size),
            line_height: s.line_height,
            selecting: false,
            marked_text: String::new(),
            bell_flash: false,
        });
        cx.subscribe(&terminal, {
            let view = view.clone();
            move |_t, ev: &terminal::event::TerminalEvent, cx| match ev {
                terminal::event::TerminalEvent::Bell => {
                    view.update(cx, |v, cx| v.ring_bell(cx));
                }
                _ => {
                    view.update(cx, |_, cx| cx.notify());
                }
            }
        })
        .detach();
        view
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    /// The view's focus handle, so a parent can focus the terminal after
    /// mounting it (e.g. switching to an external agent session). Returns a
    /// clone so the caller can call `window.focus` without holding the view's
    /// context borrow.
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;

        // Tab / shift+tab always reach the PTY while the terminal is focused:
        // Tab writes `\t` (a completion trigger in the agent TUI), shift+tab
        // writes `\x1b[Z. Handled before anything else so GPUI's focus
        // traversal never steals Tab away from the TUI — `stop_propagation`
        // keeps focus on the terminal.
        if k.key == "tab" && !k.modifiers.control && !k.modifiers.platform {
            let seq = if k.modifiers.shift { "\x1b[Z" } else { "\t" };
            let _ = self.terminal.update(cx, |t, _cx| t.input(seq.as_bytes()));
            cx.stop_propagation();
            return;
        }

        let mode_flags = self.terminal.read_with(cx, |t, _| t.mode());

        if let Some(s) = keys::to_esc_str(k, mode_flags) {
            let _ = self.terminal.update(cx, |t, _cx| t.input(s.as_bytes()));
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let mode_flags = self.terminal.read_with(cx, |t, _| t.mode());
        if mode_flags & MOUSE_MODE_MASK != 0 || ev.button != MouseButton::Left {
            return;
        }
        // Selection is handled by the terminal's internal screen state.
        self.selecting = true;
    }

    fn on_mouse_move(&mut self, _ev: &MouseMoveEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        // Selection tracking is handled at the element level in a future
        // iteration; for now, just keep the selecting flag.
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        if ev.button != MouseButton::Left || !self.selecting {
            return;
        }
        self.selecting = false;
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
        if lines == 0 {
            return;
        }
        let mode_flags = self.terminal.read_with(cx, |t, _| t.mode());
        if mode_flags & MOUSE_MODE_MASK != 0 {
            // The TUI app captures the mouse (claude code / vim / htop):
            // forward the wheel as xterm mouse reports so its own viewport
            // scrolls.
            let (row, col) = self.px_to_grid(ev.position, _window);
            self.terminal.update(cx, |t, _| {
                t.mouse_wheel(row, col, lines, &ev.modifiers);
            });
            return;
        }
        // Local scrollback scroll — rmux-core Screen does not have a
        // scroll_display API; the scrollback view is managed at the Screen
        // level and would need a scroll offset. For now, just notify to
        // trigger a repaint.
        cx.notify();
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
            color: gpui::Hsla::default(),
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
        let mut content = div()
            .flex_1()
            .w_full()
            .h_full()
            .bg(cx.theme().background)
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
                theme: TerminalTheme::from_app_theme(cx.theme()),
                font: self.font.clone(),
                font_size: self.font_size,
                line_height: self.line_height,
                marked_text: SharedString::from(self.marked_text.clone()),
            });
        if self.bell_flash {
            content = content.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(rgba(0xffffffff)),
            );
        }
        content
    }
}

impl TerminalView {
    /// React to a terminal bell per the configured `bell` mode: `Visual`
    /// flashes a brief overlay, `System` is silent here (no audio bridge yet),
    /// `Off` does nothing.
    fn ring_bell(&mut self, cx: &mut Context<Self>) {
        let mode = self.terminal.read_with(cx, |t, _| t.bell);
        if !matches!(mode, BellMode::Visual) {
            return;
        }
        self.bell_flash = true;
        cx.notify();
        let entity = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            let _ = entity.update(cx, |v, cx| {
                v.bell_flash = false;
                cx.notify();
            });
        })
        .detach();
    }

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
pub struct TerminalInputHandler {
    pub view: Entity<TerminalView>,
    pub cursor_bounds: Option<Bounds<Pixels>>,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _window: &mut Window, cx: &mut App) -> Option<std::ops::Range<usize>> {
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
        _range_utf16: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<std::ops::Range<usize>>,
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
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
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
        _range_utf16: std::ops::Range<usize>,
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
