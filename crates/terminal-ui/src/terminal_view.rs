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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    App, AppContext, Bounds, ClipboardItem, Context, Entity, FocusHandle, Font, FontFeatures,
    FontStyle, FontWeight, InputHandler, InteractiveElement, IntoElement, KeyDownEvent, Keystroke,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    Render, ScrollDelta, ScrollWheelEvent, SharedString, Styled, Subscription, Task,
    UTF16Selection, Window, div, px, rgba,
};
use gpui_component::ActiveTheme as _;
use terminal::alacritty_terminal::selection::SelectionType;
use terminal::alacritty_terminal::term::TermMode;
use terminal::alacritty_terminal::vi_mode::ViMotion;
use terminal::mappings::keys;
use terminal::mappings::mouse::{self, MouseAction};
use terminal::settings::{BellMode, CursorBlinkSetting};
use terminal::{HoverKind, HoverTarget, Rgb, Terminal};

use crate::blink::CursorBlink;
use crate::element::TerminalElement;
use crate::theme::{TerminalTheme, color_for_request, hsla_to_rgb};

/// In-flight `/pattern` search state — the pattern, the grid-coordinate match
/// ranges, and the index of the active (highlighted) match.
#[derive(Default, Clone)]
struct Search {
    pattern: String,
    matches: Vec<(terminal::Point, terminal::Point)>,
    active: usize,
}

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
    /// The element's window-space bounds from the latest prepaint, so mouse
    /// handlers can translate window positions into element-local coordinates.
    last_bounds: Option<Bounds<Pixels>>,
    /// The xterm button code of the mouse button currently held, when the TUI
    /// has captured the mouse. `None` when no button is pressed or MOUSE_MODE
    /// is not active. Used to gate MOUSE_DRAG forwarding (motion is only
    /// reported while a button is held) and to encode the release report.
    pressed_button: Option<u8>,
    /// In-flight IME marked (preedit) text, painted at the cursor by the
    /// element. Empty when no composition is active.
    marked_text: String,
    /// Open search overlay (cmd-f). `None` when closed.
    search: Option<Search>,
    /// True while a visual bell flash is active; cleared by a timer.
    bell_flash: bool,
    /// Keeps the app-level keystroke interceptor alive. While the view is
    /// focused the terminal owns the keyboard: every key outside the
    /// `commands_to_skip_shell` list is translated to the PTY before any
    /// workbench binding or focus traversal can resolve.
    key_interceptor: Option<Subscription>,
    /// Parsed `commands_to_skip_shell`: keys that skip the terminal and keep
    /// workbench precedence while it is focused.
    skip_shell: Vec<Keystroke>,
    /// Name of the process owning the foreground process group, refreshed by
    /// a 1s poll. `None` while the shell itself owns the foreground — the
    /// chip is hidden at an idle prompt.
    foreground_process: Option<String>,
    /// Keeps the foreground-process poll alive; dropping the view cancels it.
    _fg_task: Option<Task<()>>,
    /// Cursor blink phase state; ticked by `_blink_task`.
    cursor_blink: CursorBlink,
    /// Keeps the 530ms blink timer alive; dropping the view cancels it.
    _blink_task: Option<Task<()>>,
    /// Last user input time; the cursor is pinned visible for 500ms after
    /// input so typing never lands on an invisible phase.
    last_input_at: Option<Instant>,
    /// The hoverable target under the mouse (OSC 8 link / URL / path).
    /// `None` while selecting or while the TUI captures the mouse.
    hover: Option<HoverTarget>,
}

impl TerminalView {
    pub fn new(terminal: Entity<Terminal>, cx: &mut App) -> Entity<Self> {
        let terminal_for_view = terminal.clone();
        let s = terminal::settings::load();
        let skip_shell: Vec<Keystroke> = s
            .commands_to_skip_shell
            .iter()
            .filter_map(|k| gpui::Keystroke::parse(k).ok())
            .collect();
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
            last_bounds: None,
            pressed_button: None,
            marked_text: String::new(),
            search: None,
            bell_flash: false,
            key_interceptor: None,
            skip_shell,
            foreground_process: None,
            _fg_task: None,
            cursor_blink: CursorBlink::new(s.cursor_blink),
            _blink_task: None,
            last_input_at: None,
            hover: None,
        });
        cx.subscribe(&terminal, {
            let view = view.clone();
            move |_t, ev: &terminal::event::TerminalEvent, cx| match ev {
                terminal::event::TerminalEvent::Bell => {
                    view.update(cx, |v, cx| v.ring_bell(cx));
                }
                terminal::event::TerminalEvent::ColorRequest(idx, fmt) => {
                    view.update(cx, |v, cx| v.answer_color_request(*idx, fmt.clone(), cx));
                }
                terminal::event::TerminalEvent::CursorBlinkingChange => {
                    // The program flipped its blink flag: restart the phase
                    // visible so the cursor never vanishes on the toggle.
                    view.update(cx, |v, cx| {
                        v.cursor_blink.reset();
                        cx.notify();
                    });
                }
                _ => {
                    view.update(cx, |_, cx| cx.notify());
                }
            }
        })
        .detach();

        // While focused, the terminal owns the keyboard: this interceptor
        // runs before any binding or focus traversal resolves, translating
        // every key through the general PTY pipeline (see
        // `handle_terminal_key`). Only `commands_to_skip_shell` entries keep
        // workbench precedence.
        let interceptor = {
            let weak = view.downgrade();
            cx.intercept_keystrokes(move |ev, window, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                let focused = view
                    .read_with(cx, |v, _| v.focus_handle.clone())
                    .is_focused(window);
                if !focused {
                    return;
                }
                let k = ev.keystroke.clone();
                let skip = view.read_with(cx, |v, _| {
                    v.skip_shell
                        .iter()
                        .any(|p| p.key == k.key && p.modifiers == k.modifiers)
                });
                if skip {
                    return;
                }
                let consumed = view.update(cx, |v, cx| v.handle_terminal_key(&k, cx));
                if consumed {
                    cx.stop_propagation();
                }
            })
        };
        view.update(cx, |v, _| v.key_interceptor = Some(interceptor));
        view.update(cx, |v, cx| {
            v.start_foreground_poll(cx);
            v.start_cursor_blink(cx);
        });
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
        // Fallback for skip-shell keys whose workbench binding did not fire;
        // the interceptor handles everything else before bindings resolve.
        let _ = self.handle_terminal_key(&ev.keystroke, cx);
    }

    /// Terminal-first key handling: search overlay, paste/copy, vi mode, and
    /// the general `keys::to_esc_str` PTY translation. Returns `true` when the
    /// key was consumed (the interceptor then stops propagation so no
    /// workbench binding or focus traversal can steal it).
    fn handle_terminal_key(&mut self, k: &Keystroke, cx: &mut Context<Self>) -> bool {
        // cmd/ctrl-f toggles the search overlay.
        if (k.modifiers.platform || k.modifiers.control) && k.key == "f" {
            if self.search.is_some() {
                self.search = None;
            } else {
                self.search = Some(Search::default());
            }
            cx.notify();
            return true;
        }

        // While the search overlay is open, keystrokes edit the pattern (the
        // TUI does not receive them). esc closes; enter closes; Tab closes and
        // sends the horizontal-tab byte to the PTY via the SendTab action;
        // cmd-g would cycle but is left to vi mode's own search for now.
        if self.search.is_some() {
            match k.key.as_ref() {
                "escape" | "enter" | "return" => {
                    self.search = None;
                    cx.notify();
                    return true;
                }
                "backspace" => {
                    if let Some(search) = self.search.as_mut() {
                        search.pattern.pop();
                    }
                    self.run_search(cx);
                    return true;
                }
                "tab" if !k.modifiers.control && !k.modifiers.platform => {
                    // Close the overlay, then fall through to the general PTY
                    // translation below (sends \t).
                    self.search = None;
                    cx.notify();
                }
                _ => {
                    // Append a single printable char to the pattern.
                    if !k.modifiers.control && !k.modifiers.platform {
                        let mut chars = k.key.chars();
                        if let Some(c) = chars.next()
                            && chars.next().is_none()
                            && c.is_ascii()
                            && !c.is_ascii_control()
                        {
                            let ch = if k.modifiers.shift && c.is_ascii_alphabetic() {
                                c.to_ascii_uppercase()
                            } else {
                                c
                            };
                            if let Some(search) = self.search.as_mut() {
                                search.pattern.push(ch);
                            }
                            self.run_search(cx);
                        }
                    }
                    return true;
                }
            }
        }

        // Paste: cmd-v on mac, ctrl-v elsewhere.
        #[cfg(target_os = "macos")]
        let paste = k.modifiers.platform && k.key == "v";
        #[cfg(not(target_os = "macos"))]
        let paste = k.modifiers.control && !k.modifiers.shift && !k.modifiers.alt && k.key == "v";
        if paste {
            if let Some(item) = cx.read_from_clipboard()
                && let Some(text) = item.text()
                && !text.is_empty()
            {
                self.terminal.read_with(cx, |t, _| {
                    let _ = t.paste(&text);
                });
                self.note_input(cx);
            }
            return true;
        }

        // Copy: cmd-c on mac, ctrl-c elsewhere. While the terminal is focused
        // the reclaimed-keys whitelist shadows gpui-component Root's
        // window-wide Copy binding, so these keys land here. With no
        // selection: no-op on mac, `^C` elsewhere so interrupt stays reachable.
        #[cfg(target_os = "macos")]
        let copy = k.modifiers.platform && k.key == "c";
        #[cfg(not(target_os = "macos"))]
        let copy = k.modifiers.control && !k.modifiers.shift && !k.modifiers.alt && k.key == "c";
        if copy {
            match self.terminal.read_with(cx, |t, _| t.selection_to_string()) {
                Some(text) if !text.is_empty() => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    self.terminal.update(cx, |t, _| t.clear_selection());
                    cx.notify();
                }
                _ => {
                    #[cfg(not(target_os = "macos"))]
                    {
                        let _ = self.terminal.update(cx, |t, _| t.input(b"\x03"));
                    }
                }
            }
            return true;
        }

        // Toggle the terminal's built-in vi mode (alacritty's, not `vim`)
        // on ctrl+shift+v.
        if k.modifiers.control && k.modifiers.shift && k.key == "v" {
            self.terminal.update(cx, |t, cx| t.toggle_vi_mode(cx));
            return true;
        }

        let mode = self.terminal.read_with(cx, |t, _| t.mode());

        // In vi mode, motion keys move the vi cursor and are NOT forwarded
        // to the PTY; unmapped keys are swallowed.
        if mode.contains(TermMode::VI) {
            if let Some(motion) = vi_motion_for(k) {
                self.terminal.update(cx, |t, cx| t.vi_motion(motion, cx));
            }
            return true;
        }

        // Unbound cmd/super combos produce no PTY input; without this guard the
        // printable branch would type a raw char for e.g. cmd-x. `platform` is
        // cmd/super/win (never ctrl, see gpui `Modifiers`), and the extra
        // `!control` keeps ctrl-combos flowing to the control-char branch.
        if k.modifiers.platform && !k.modifiers.control {
            return true;
        }

        if let Some(s) = keys::to_esc_str(k, mode) {
            let _ = self.terminal.update(cx, |t, _cx| t.input(s.as_bytes()));
            self.note_input(cx);
            return true;
        }

        // Bare modifiers / unknown keys: nothing to send.
        false
    }

    /// Run the current search pattern against the terminal grid and store the
    /// matches so the element can highlight them.
    fn run_search(&mut self, cx: &mut Context<Self>) {
        let pattern = self
            .search
            .as_ref()
            .map(|s| s.pattern.clone())
            .unwrap_or_default();
        if pattern.is_empty() {
            if let Some(search) = self.search.as_mut() {
                search.matches.clear();
                search.active = 0;
            }
            cx.notify();
            return;
        }
        let matches = self
            .terminal
            .read_with(cx, |t, _| t.search_matches(&pattern).unwrap_or_default());
        if let Some(search) = self.search.as_mut() {
            search.matches = matches;
            search.active = 0;
        }
        cx.notify();
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let (row, col) = self.px_to_grid(ev.position, window);

        // cmd/ctrl+click opens the target under the cursor: an OSC 8
        // hyperlink first, then a hovered URL, then a hovered path.
        if ev.modifiers.platform || ev.modifiers.control {
            let target = self.terminal.read_with(cx, |t, _| {
                t.hyperlink_at(row, col)
                    .map(|url| (url, HoverKind::Url))
                    .or_else(|| t.hover_target(row, col).map(|h| (h.text, h.kind)))
                    .map(|(text, kind)| (text, kind, t.cwd.clone()))
            });
            if let Some((text, kind, cwd)) = target {
                open_target(&text, kind, &cwd);
                return;
            }
        }

        let ty = selection_type_for(ev.click_count);
        let mode = self.terminal.read_with(cx, |t, _| t.mode());

        if mode.intersects(TermMode::MOUSE_MODE) {
            if ev.modifiers.shift {
                // Shift overrides mouse mode: start local selection so the
                // user can select text even when the TUI has captured the
                // mouse.
                self.terminal
                    .update(cx, |t, cx| t.start_selection(ty, row, col, cx));
                self.selecting = true;
                self.hover = None;
                return;
            }
            // Forward the click to the TUI app as an xterm mouse report.
            let button = mouse_button_code(ev.button) | modifier_bits(&ev.modifiers);
            if let Some(report) =
                mouse::encode(button, MouseAction::Press, col as u32, row as u32, mode)
            {
                let _ = self.terminal.update(cx, |t, _| t.input(&report));
                self.pressed_button = Some(button);
            }
            return;
        }

        if ev.button != MouseButton::Left {
            return;
        }
        self.terminal
            .update(cx, |t, cx| t.start_selection(ty, row, col, cx));
        self.selecting = true;
        self.hover = None;
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            let (row, col) = self.px_to_grid(ev.position, window);
            self.terminal
                .update(cx, |t, cx| t.update_selection(row, col, cx));
            self.copy_selection_live(cx);
            return;
        }
        // Forward motion to the TUI. MOUSE_MOTION reports motion whenever
        // the mouse moves; MOUSE_DRAG only reports motion while a button is
        // held (tracked via `pressed_button`).
        let mode = self.terminal.read_with(cx, |t, _| t.mode());
        let can_report = mode.contains(TermMode::MOUSE_MOTION)
            || (mode.contains(TermMode::MOUSE_DRAG) && self.pressed_button.is_some());
        if can_report {
            let (row, col) = self.px_to_grid(ev.position, window);
            let button = self.pressed_button.unwrap_or(0) | modifier_bits(&ev.modifiers);
            if let Some(report) =
                mouse::encode(button, MouseAction::Motion, col as u32, row as u32, mode)
            {
                let _ = self.terminal.update(cx, |t, _| t.input(&report));
            }
        }
        // Track the hoverable target under the mouse while the TUI does not
        // capture it; the element underlines the span and the view shows the
        // tooltip / cmd+click opens it.
        if mode.intersects(TermMode::MOUSE_MODE) {
            if self.hover.is_some() {
                self.hover = None;
                cx.notify();
            }
        } else {
            let (row, col) = self.px_to_grid(ev.position, window);
            let target = self.terminal.read_with(cx, |t, _| t.hover_target(row, col));
            if target != self.hover {
                self.hover = target;
                cx.notify();
            }
        }
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.finalize_selection(cx);
            return;
        }
        // Forward release to the TUI if it has captured the mouse. Clear
        // pressed_button even when mouse mode is off so a stale value does
        // not gate MOUSE_DRAG if the TUI re-enables it later.
        let button = self.pressed_button.take().unwrap_or(0);
        let mode = self.terminal.read_with(cx, |t, _| t.mode());
        if mode.intersects(TermMode::MOUSE_MODE) {
            let (row, col) = self.px_to_grid(ev.position, window);
            let button = button | modifier_bits(&ev.modifiers);
            if let Some(report) =
                mouse::encode(button, MouseAction::Release, col as u32, row as u32, mode)
            {
                let _ = self.terminal.update(cx, |t, _| t.input(&report));
            }
        }
    }

    fn on_scroll_wheel(
        &mut self,
        ev: &ScrollWheelEvent,
        window: &mut Window,
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
        let mode = self.terminal.read_with(cx, |t, _| t.mode());
        if mode.intersects(TermMode::MOUSE_MODE) {
            // The TUI app captures the mouse (claude code / vim / htop): forward
            // the wheel as xterm mouse reports so its own viewport scrolls.
            // Local scrollback scroll is a no-op on the alt screen the TUI
            // owns, so without this the wheel does nothing.
            let (row, col) = self.px_to_grid(ev.position, window);
            self.terminal.update(cx, |t, _| {
                t.mouse_wheel(row, col, lines, &ev.modifiers);
            });
            return;
        }
        if mode.intersects(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            // Alt screen without mouse capture (less, git log): the wheel
            // becomes arrow-key presses (xterm alternateScroll). With the
            // mode off the wheel stays dead here — local scrollback is a
            // no-op on the alt screen anyway.
            self.terminal.update(cx, |t, _| t.alternate_scroll(lines));
            return;
        }
        self.terminal.update(cx, |t, cx| t.scroll(lines, cx));
    }

    /// Map an element-relative pixel position to `(row, col)` grid coords by
    /// measuring the monospace cell width from the same font the element
    /// paints with.
    fn px_to_grid(&self, pos: Point<Pixels>, window: &Window) -> (usize, usize) {
        let cell_w = self.cell_width(window);
        let line_h = px(f32::from(self.font_size) * self.line_height);
        let origin = self.last_bounds.map(|b| b.origin).unwrap_or_default();
        grid_from_px(pos, origin, cell_w, line_h)
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (search_matches, active_match, pattern, count) = self
            .search
            .as_ref()
            .map(|s| {
                (
                    s.matches.clone(),
                    Some(s.active),
                    s.pattern.clone(),
                    s.matches.len(),
                )
            })
            .unwrap_or_default();

        let overlay = if !pattern.is_empty() {
            Some(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .px_2()
                    .py_1()
                    .bg(cx.theme().background)
                    .child(div().text_xs().text_color(cx.theme().foreground).child(
                        agent::i18n::t_str_count(
                            "terminal-search-status",
                            &[("pattern", pattern.as_str())],
                            count as i64,
                        ),
                    )),
            )
        } else {
            None
        };

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
                search_matches,
                active_match,
                hover: self.hover.clone(),
                cursor_visible: self.cursor_visible(cx),
            });
        if let Some(o) = overlay {
            content = content.child(o);
        }
        // Starting indicator: shown until the shell / agent TUI reports ready
        // (marker tap, quiet window, or fallback — see `Terminal::is_ready`).
        if !self.terminal.read_with(cx, |t, _| t.is_ready()) {
            content = content.child(
                div().absolute().top_0().right_0().px_2().py_1().child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(agent::i18n::t("terminal-starting")),
                ),
            );
        }
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
        // Foreground-process chip: the running program's name at the bottom
        // right while something other than the shell owns the foreground.
        if let Some(name) = &self.foreground_process {
            content = content.child(
                div().absolute().bottom_0().right_0().px_2().py_1().child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(name.clone()),
                ),
            );
        }
        // Hover tooltip: the target text, anchored under the hovered span.
        if let Some(hover) = &self.hover {
            let origin = self.last_bounds.map(|b| b.origin).unwrap_or_default();
            let cell_w = self.cell_width(window);
            let line_h = px(f32::from(self.font_size) * self.line_height);
            let x = origin.x + hover.start_col as f32 * cell_w;
            let y = origin.y + (hover.row + 1) as f32 * line_h;
            content = content.child(
                div()
                    .absolute()
                    .left(x)
                    .top(y)
                    .px_2()
                    .py_1()
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().popover_foreground)
                            .child(hover.text.clone()),
                    ),
            );
        }
        content
    }
}

impl TerminalView {
    /// Answer an OSC 10/11/12 color query from the active theme. Indices past
    /// the cursor slot are not ours and go unanswered.
    fn answer_color_request(
        &mut self,
        idx: usize,
        fmt: Arc<dyn Fn(Rgb) -> String + Send + Sync + 'static>,
        cx: &mut Context<Self>,
    ) {
        let theme = TerminalTheme::from_app_theme(cx.theme());
        let Some(color) = color_for_request(&theme, idx) else {
            return;
        };
        let response = fmt(hsla_to_rgb(color));
        let _ = self
            .terminal
            .read_with(cx, |t, _| t.input(response.as_bytes()));
    }

    /// Poll the foreground process once a second and keep
    /// `foreground_process` current, notifying only on change. The stored
    /// task is cancelled when the view drops; the loop also self-terminates
    /// if either side of the update channel is gone.
    fn start_foreground_poll(&mut self, cx: &mut Context<Self>) {
        let terminal = self.terminal.clone();
        self._fg_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let name = terminal.read_with(cx, |t, _| t.foreground_process_name());
                if this
                    .update(cx, |v, cx| {
                        if v.foreground_process != name {
                            v.foreground_process = name;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

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
        self.note_input(cx);
    }

    /// Record user input: pins the cursor visible for 500ms and resets the
    /// blink phase so typing never lands on an invisible cursor.
    fn note_input(&mut self, cx: &mut Context<Self>) {
        self.last_input_at = Some(Instant::now());
        self.cursor_blink.reset();
        cx.notify();
    }

    /// Whether the cursor paints this frame: the blink phase combined with
    /// the program's blink flag, pinned visible while selecting, composing
    /// IME text, or within 500ms of the last input.
    fn cursor_visible(&self, cx: &App) -> bool {
        let term_blinking = self.terminal.read_with(cx, |t, _| t.cursor_blinking());
        let force = self.selecting
            || !self.marked_text.is_empty()
            || self
                .last_input_at
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(500));
        self.cursor_blink.visible(term_blinking, force)
    }

    /// Tick the blink phase every 530ms. A phase flip only repaints when the
    /// blink is live (mode `On`, or `Terminal` while the program's blink flag
    /// is set); the stored task dies with the view.
    fn start_cursor_blink(&mut self, cx: &mut Context<Self>) {
        let terminal = self.terminal.clone();
        self._blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(530))
                    .await;
                if this
                    .update(cx, |v, cx| {
                        v.cursor_blink.tick();
                        let live = match v.cursor_blink.mode() {
                            CursorBlinkSetting::Off => false,
                            CursorBlinkSetting::On => true,
                            CursorBlinkSetting::Terminal => {
                                terminal.read_with(cx, |t, _| t.cursor_blinking())
                            }
                        };
                        if live {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// Select-to-copy: mirror the in-flight selection into the clipboard on
    /// every drag move, so the text is captured even when the release happens
    /// outside the window (where no mouse-up reaches us).
    fn copy_selection_live(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.terminal.read_with(cx, |t, _| t.selection_to_string()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Whether a mouse-driven text selection is in flight; the element uses it
    /// to decide whether to register the window-level mouse-up listener.
    pub(crate) fn is_selecting(&self) -> bool {
        self.selecting
    }

    /// Record the element's window-space bounds (written back by the element
    /// each prepaint) so mouse positions can be made element-local.
    pub(crate) fn set_last_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.last_bounds = Some(bounds);
    }

    /// End an in-flight selection and copy it to the clipboard
    /// (select-to-copy). Idempotent: the div's `on_mouse_up` and the
    /// window-level mouse-up listener both route here; the `selecting` flag
    /// gates the second call.
    pub(crate) fn finalize_selection(&mut self, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        self.selecting = false;
        if let Some(text) = self.terminal.read_with(cx, |t, _| t.selection_to_string()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.terminal.update(cx, |t, _| t.clear_selection());
        }
        cx.notify();
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

/// Map a window-space pixel position to `(row, col)` grid coordinates relative
/// to the element's window-space origin. Positions left/above the origin clamp
/// to 0 so drags outside the element stay bounded.
fn grid_from_px(
    pos: Point<Pixels>,
    origin: Point<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
) -> (usize, usize) {
    let col = (f32::from(pos.x - origin.x) / f32::from(cell_w))
        .floor()
        .max(0.) as usize;
    let row = (f32::from(pos.y - origin.y) / f32::from(line_h))
        .floor()
        .max(0.) as usize;
    (row, col)
}

/// Selection granularity by click count: 1 = char, 2 = word (semantic),
/// 3 = line. Counts past 3 fall back to char selection.
fn selection_type_for(click_count: usize) -> SelectionType {
    match click_count {
        2 => SelectionType::Semantic,
        3 => SelectionType::Lines,
        _ => SelectionType::Simple,
    }
}

/// Open a cmd/ctrl+click target: URLs in the browser; paths revealed in the
/// file manager (directories opened directly). A leading `~/` expands to the
/// home directory; relative paths resolve against the terminal's cwd.
fn open_target(text: &str, kind: HoverKind, cwd: &Path) {
    let mut cmd = std::process::Command::new("open");
    match kind {
        HoverKind::Url => {
            cmd.arg(text);
        }
        HoverKind::Path => {
            let path = match text.strip_prefix("~/") {
                Some(rest) => std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(rest))
                    .unwrap_or_else(|| PathBuf::from(text)),
                None => PathBuf::from(text),
            };
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            if !path.is_dir() {
                cmd.arg("-R");
            }
            cmd.arg(path);
        }
    }
    let _ = cmd.spawn();
}

/// Map a gpui `MouseButton` to an xterm button code (left=0, middle=1,
/// right=2). Unrecognised buttons map to 0 (left) so the click is still
/// forwarded rather than silently dropped.
fn mouse_button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        _ => 0,
    }
}

/// xterm modifier bits: shift=4, alt=8, control=16. Matches the encoding in
/// `Terminal::mouse_wheel` (term.rs:278).
fn modifier_bits(modifiers: &gpui::Modifiers) -> u8 {
    (4 * modifiers.shift as u8) + (8 * modifiers.alt as u8) + (16 * modifiers.control as u8)
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    #[test]
    fn grid_from_px_subtracts_origin() {
        let (row, col) = grid_from_px(
            point(px(100.), px(50.)),
            point(px(40.), px(20.)),
            px(8.),
            px(16.),
        );
        assert_eq!((row, col), (1, 7));
    }

    #[test]
    fn grid_from_px_clamps_negative_to_zero() {
        let (row, col) = grid_from_px(
            point(px(10.), px(5.)),
            point(px(40.), px(20.)),
            px(8.),
            px(16.),
        );
        assert_eq!((row, col), (0, 0));
    }

    #[test]
    fn grid_from_px_without_origin() {
        let (row, col) = grid_from_px(point(px(16.), px(32.)), Point::default(), px(8.), px(16.));
        assert_eq!((row, col), (2, 2));
    }

    #[test]
    fn click_count_maps_to_selection_granularity() {
        assert_eq!(selection_type_for(1), SelectionType::Simple);
        assert_eq!(selection_type_for(2), SelectionType::Semantic);
        assert_eq!(selection_type_for(3), SelectionType::Lines);
        assert_eq!(selection_type_for(4), SelectionType::Simple);
    }
}
