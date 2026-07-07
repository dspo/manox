//! `TerminalView` — the gpui `Render` wrapper around `TerminalElement`.
//!
//! Owns an `Entity<Terminal>`, renders the element full-bleed, and routes
//! keyboard input to the terminal's PTY. Stage 2 handles the common ASCII
//! range plus Enter/Backspace/Escape/arrow keys; stage 3 replaces this with
//! the full `mappings::keys::to_esc_str` table (modifier combos, keypad,
//! application cursor mode, bracketed paste).

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, Styled, Window, black, div,
};
use terminal::Terminal;
use terminal::mappings::keys;

use crate::element::TerminalElement;

/// A view that hosts one terminal session. Created by the workspace when the
/// user opens a terminal tab.
pub struct TerminalView {
    terminal: Entity<Terminal>,
    focus_handle: FocusHandle,
}

impl TerminalView {
    pub fn new(terminal: Entity<Terminal>, cx: &mut App) -> Entity<Self> {
        let terminal_for_view = terminal.clone();
        let view = cx.new(move |cx| Self {
            terminal: terminal_for_view,
            focus_handle: cx.focus_handle(),
        });
        // Re-render whenever the terminal emits a wake-up (new PTY output,
        // cursor move, bell, etc.). The element reads a fresh snapshot each
        // frame, so a notify is enough. On `App` the subscribe callback takes
        // `(entity, event, cx)` — no `this`, since `App` owns no state.
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
        // Branch on APP_CURSOR / APP_KEYPAD so vim, mc, and readline all get
        // the escape sequence their mode expects.
        let mode = self.terminal.read_with(cx, |t, _| t.mode());
        if let Some(s) = keys::to_esc_str(&ev.keystroke, mode) {
            let _ = self.terminal.update(cx, |t, _cx| t.input(s.as_bytes()));
        }
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
            .child(TerminalElement::new(self.terminal.clone()))
    }
}
