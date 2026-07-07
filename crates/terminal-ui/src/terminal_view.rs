//! `TerminalView` — the gpui `Render` wrapper around `TerminalElement`.
//!
//! Owns an `Entity<Terminal>`, renders the element full-bleed, and routes
//! keyboard input to the terminal's PTY. Stage 2 handles the common ASCII
//! range plus Enter/Backspace/Escape/arrow keys; stage 3 replaces this with
//! the full `mappings::keys::to_esc_str` table (modifier combos, keypad,
//! application cursor mode, bracketed paste).

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    Keystroke, ParentElement, Render, Styled, Window, black, div,
};
use terminal::Terminal;

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

    /// Translate a keystroke to the bytes the PTY expects and write them.
    /// Stage-2 subset; stage 3 swaps in `mappings::keys::to_esc_str`.
    fn keystroke_to_bytes(keystroke: &Keystroke) -> Vec<u8> {
        if keystroke.key.is_empty() {
            return Vec::new();
        }

        let bytes: Vec<u8> = match keystroke.key.as_ref() {
            "enter" | "return" => b"\r".to_vec(),
            "backspace" => b"\x7f".to_vec(),
            "tab" => b"\t".to_vec(),
            "escape" => b"\x1b".to_vec(),
            "up" => b"\x1b[A".to_vec(),
            "down" => b"\x1b[B".to_vec(),
            "right" => b"\x1b[C".to_vec(),
            "left" => b"\x1b[D".to_vec(),
            "home" => b"\x1b[H".to_vec(),
            "end" => b"\x1b[F".to_vec(),
            "pageup" => b"\x1b[5~".to_vec(),
            "pagedown" => b"\x1b[6~".to_vec(),
            "delete" => b"\x1b[3~".to_vec(),
            "space" => b" ".to_vec(),
            _ => {
                let mut chars = keystroke.key.chars();
                let first = match chars.next() {
                    Some(c) if chars.next().is_none() => c,
                    _ => return Vec::new(),
                };
                if first.is_ascii() {
                    let c = if keystroke.modifiers.control {
                        let lower = first.to_ascii_lowercase();
                        let code = (lower as u8).wrapping_sub(b'a').wrapping_add(1);
                        code as char
                    } else {
                        first
                    };
                    c.to_string().into_bytes()
                } else {
                    // Non-ASCII (CJK etc.) — stage 6 routes through IME; pass
                    // the raw codepoint through for now.
                    first.to_string().into_bytes()
                }
            }
        };
        bytes
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let bytes = Self::keystroke_to_bytes(&ev.keystroke);
        if !bytes.is_empty() {
            let _ = self.terminal.update(cx, |t, _cx| t.input(&bytes));
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
