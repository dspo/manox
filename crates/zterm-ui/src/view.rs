use std::path::PathBuf;
use std::ops::Range as StdRange;
use std::time::{Duration, Instant};

use gpui::{
    Context, Entity, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseUpEvent,
    ScrollWheelEvent, Subscription, Task, Window, div, prelude::*,
};
use zterm_core::{
    AlternateScroll, CursorShape, Event as TerminalEvent, MaybeNavigationTarget, PathLikeTarget,
    Terminal, TerminalBuilder, ToggleViMode,
};
use util::ResultExt as _;
use util::paths::PathStyle;

use crate::element::TerminalElement;

const OPTION_AS_META: bool = false;
const SCROLL_MULTIPLIER: f32 = 1.0;
const HYPERLINK_TIMEOUT: Duration = Duration::from_millis(1);

pub struct TerminalView {
    terminal: Option<Entity<Terminal>>,
    spawn_error: Option<String>,
    focus_handle: FocusHandle,
    needs_initial_focus: bool,
    ime_state: Option<ImeState>,
    blinking: bool,
    blink_on: bool,
    last_activity: Instant,
    search_active: bool,
    search_query: String,
    match_count: usize,
    active_match: usize,
    search_task: Option<Task<()>>,
    _blink_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// In-progress IME composition (pre-edit) text, not yet committed to the PTY.
#[derive(Default)]
struct ImeState {
    marked_text: String,
}

impl TerminalView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let mut this = Self {
            terminal: None,
            spawn_error: None,
            focus_handle,
            needs_initial_focus: true,
            ime_state: None,
            blinking: false,
            blink_on: true,
            last_activity: Instant::now(),
            search_active: false,
            search_query: String::new(),
            match_count: 0,
            active_match: 0,
            search_task: None,
            _blink_task: None,
            _subscriptions: Vec::new(),
        };
        this.spawn_shell(cx);
        this._blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let alive = this
                    .update(cx, |this, cx| {
                        let paused = this.last_activity.elapsed() < Duration::from_millis(500);
                        if this.blinking && !paused {
                            this.blink_on = !this.blink_on;
                            cx.notify();
                        } else if !this.blink_on && (!this.blinking || paused) {
                            this.blink_on = true;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Whether the cursor should currently be drawn (blink phase + enabled).
    pub(crate) fn cursor_blink_visible(&self) -> bool {
        !self.blinking || self.blink_on
    }

    fn mark_active(&mut self, cx: &mut Context<Self>) {
        self.last_activity = Instant::now();
        if !self.blink_on {
            self.blink_on = true;
            cx.notify();
        }
    }

    pub(crate) fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return self.clear_marked_text(cx);
        }
        self.ime_state = Some(ImeState { marked_text: text });
        cx.notify();
    }

    pub(crate) fn marked_text_range(&self) -> Option<StdRange<usize>> {
        self.ime_state
            .as_ref()
            .map(|state| 0..state.marked_text.encode_utf16().count())
    }

    pub(crate) fn marked_text(&self) -> Option<String> {
        self.ime_state.as_ref().map(|state| state.marked_text.clone())
    }

    pub(crate) fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.ime_state.is_some() {
            self.ime_state = None;
            cx.notify();
        }
    }

    pub(crate) fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.clear_marked_text(cx);
        if !text.is_empty()
            && let Some(terminal) = &self.terminal
        {
            terminal.update(cx, |terminal, _| {
                terminal.input(text.to_string().into_bytes());
            });
        }
    }

    fn spawn_shell(&mut self, cx: &mut Context<Self>) {
        let (completion_tx, completion_rx) = async_channel::unbounded();
        let builder_task = TerminalBuilder::new(
            None,
            None,
            std::env::vars().collect(),
            CursorShape::default(),
            AlternateScroll::On,
            None,
            path_hyperlink_regexes(),
            HYPERLINK_TIMEOUT,
            0,
            Some(completion_tx),
            cx,
            PathStyle::local(),
        );

        cx.spawn(async move |this, cx| {
            let builder = match builder_task.await {
                Ok(builder) => builder,
                Err(error) => {
                    eprintln!("zterm: failed to start shell: {error:#}");
                    this.update(cx, |this, cx| {
                        this.spawn_error = Some(format!("{error:#}"));
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            };
            let terminal = cx.update(|cx| cx.new(|cx| builder.subscribe(cx)));
            this.update(cx, |this, cx| {
                this._subscriptions
                    .push(cx.observe(&terminal, |_, _, cx| cx.notify()));
                this._subscriptions
                    .push(cx.subscribe(&terminal, Self::on_terminal_event));
                this.terminal = Some(terminal);
                cx.notify();
            })
            .log_err();
        })
        .detach();

        cx.spawn(async move |_, cx| {
            // Err means the channel closed without a shell exit (spawn
            // failure); stay up so the error remains visible.
            if completion_rx.recv().await.is_ok() {
                cx.update(|cx| cx.quit());
            }
        })
        .detach();
    }

    fn on_terminal_event(
        &mut self,
        _terminal: Entity<Terminal>,
        event: &TerminalEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalEvent::Open(target) => {
                let argument = open_argument(target.clone());
                cx.spawn(async move |_, _| {
                    smol::process::Command::new("open")
                        .arg(argument)
                        .status()
                        .await
                        .log_err();
                })
                .detach();
            }
            TerminalEvent::BlinkChanged(blinking) => {
                self.blinking = *blinking;
                if !*blinking {
                    self.blink_on = true;
                }
                cx.notify();
            }
            TerminalEvent::CloseTerminal => cx.quit(),
            // Content-affecting events (Wakeup on PTY output, selection, title,
            // blink) must invalidate the view so output appears immediately.
            _ => cx.notify(),
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.mark_active(cx);
        let keystroke = event.keystroke.clone();

        if keystroke.modifiers.platform && keystroke.key == "f" {
            self.toggle_search(cx);
            cx.stop_propagation();
            return;
        }

        if self.search_active {
            self.handle_search_key(&keystroke, cx);
            cx.stop_propagation();
            return;
        }

        if keystroke.modifiers.platform && !keystroke.modifiers.shift {
            match keystroke.key.as_str() {
                "c" => {
                    self.copy(cx);
                    cx.stop_propagation();
                    return;
                }
                "v" => {
                    self.paste(cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if keystroke.modifiers.platform
            && keystroke.modifiers.control
            && keystroke.key == "space"
        {
            window.show_character_palette();
            cx.stop_propagation();
            return;
        }

        if let Some(terminal) = self.terminal.clone() {
            let handled = terminal.update(cx, |terminal, _cx| {
                terminal.try_keystroke(&keystroke, OPTION_AS_META)
            });
            if handled {
                cx.stop_propagation();
            }
        }
    }

    fn toggle_vi(&mut self, _action: &ToggleViMode, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, _| terminal.toggle_vi_mode());
            self.mark_active(cx);
        }
    }
    fn toggle_search(&mut self, cx: &mut Context<Self>) {
        self.search_active = !self.search_active;
        if !self.search_active {
            self.search_query.clear();
            self.clear_search_matches(cx);
        } else {
            self.update_search(cx);
        }
        cx.notify();
    }

    fn clear_search_matches(&mut self, cx: &mut Context<Self>) {
        self.match_count = 0;
        self.active_match = 0;
        if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, _| terminal.select_matches(&[]));
        }
    }

    fn handle_search_key(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        match keystroke.key.as_str() {
            "escape" => {
                self.search_active = false;
                self.search_query.clear();
                self.clear_search_matches(cx);
                cx.notify();
            }
            "enter" => {
                let delta = if keystroke.modifiers.shift { -1 } else { 1 };
                self.step_match(delta, cx);
            }
            "backspace" => {
                self.search_query.pop();
                self.update_search(cx);
            }
            key if !keystroke.modifiers.platform
                && !keystroke.modifiers.control
                && key.chars().count() == 1 =>
            {
                self.search_query.push_str(key);
                self.update_search(cx);
            }
            _ => {}
        }
    }

    fn step_match(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.match_count == 0 {
            return;
        }
        let next = (self.active_match as isize + delta).rem_euclid(self.match_count as isize);
        self.active_match = next as usize;
        if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, _| terminal.activate_match(self.active_match));
        }
    }

    fn update_search(&mut self, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.clone() else {
            return;
        };
        let query = self.search_query.clone();
        let search = if query.is_empty() {
            None
        } else {
            zterm_core::Search::new(&regex::escape(&query))
        };
        self.search_task = Some(cx.spawn(async move |this, cx| {
            let matches = if let Some(search) = search {
                let task = terminal.update(cx, |terminal, cx| terminal.find_matches(search, cx));
                task.await
            } else {
                Vec::new()
            };
            this.update(cx, |this, cx| {
                if let Some(terminal) = &this.terminal {
                    terminal.update(cx, |terminal, _| terminal.select_matches(&matches));
                }
                this.match_count = matches.len();
                this.active_match = 0;
                cx.notify();
            })
            .ok();
        }));
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, _| terminal.copy(None));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, _| terminal.paste(&text));
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.mark_active(cx);
        self.focus_handle.focus(window, cx);
        if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, cx| terminal.mouse_down(event, cx));
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, cx| terminal.mouse_up(event, cx));
        }
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, _| {
                terminal.scroll_wheel(event, SCROLL_MULTIPLIER)
            });
            // Scroll deltas are queued as internal events and only applied in
            // sync() during the next prepaint; request that redraw immediately or
            // scrolling appears frozen until an unrelated repaint.
            cx.notify();
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.needs_initial_focus && self.terminal.is_some() {
            self.focus_handle.focus(window, cx);
            self.needs_initial_focus = false;
        }

        if let Some(error) = &self.spawn_error {
            return div()
                .size_full()
                .bg(zterm_core::TERMINAL_BACKGROUND)
                .text_color(gpui::red())
                .child(format!("Failed to start shell: {error}"))
                .into_any_element();
        }

        let Some(terminal) = self.terminal.clone() else {
            return div()
                .size_full()
                .bg(zterm_core::TERMINAL_BACKGROUND)
                .text_color(gpui::white())
                .child("Starting shell…")
                .into_any_element();
        };

        let vi = terminal.read(cx).vi_mode_enabled();

        div()
            .id("zterm-view")
            .relative()
            .size_full()
            .bg(zterm_core::TERMINAL_BACKGROUND)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .on_action(cx.listener(Self::toggle_vi))
            .child(TerminalElement::new(
                terminal,
                cx.entity(),
                self.focus_handle.clone(),
                self.focus_handle.is_focused(window),
                self.marked_text(),
            ))
            .when(vi, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_1()
                        .right_2()
                        .text_xs()
                        .text_color(gpui::yellow())
                        .child("VI"),
                )
            })
            .when(self.search_active, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_1()
                        .right_2()
                        .px_2()
                        .py_1()
                        .bg(gpui::black())
                        .text_xs()
                        .text_color(gpui::white())
                        .child(format!(
                            "/{}  {}/{}",
                            self.search_query,
                            if self.match_count == 0 {
                                0
                            } else {
                                self.active_match + 1
                            },
                            self.match_count
                        )),
                )
            })
            .into_any_element()
    }
}

fn open_argument(target: MaybeNavigationTarget) -> String {
    match target {
        MaybeNavigationTarget::Url(url) => url,
        MaybeNavigationTarget::PathLike(path_like) => {
            resolve_path_like(&path_like).to_string_lossy().into_owned()
        }
    }
}

/// Resolve a path-like navigation target to a concrete path, stripping
/// `:line(:column)` suffixes and anchoring relative paths at the terminal's
/// working directory. Falls back to the raw string when nothing exists.
fn resolve_path_like(target: &PathLikeTarget) -> PathBuf {
    let mut candidate = target.maybe_path.as_str();
    loop {
        let path = PathBuf::from(candidate);
        let anchored = if path.is_absolute() {
            path
        } else if let Some(working_directory) = &target.working_directory {
            working_directory.join(&path)
        } else {
            path
        };
        if anchored.exists() {
            return anchored;
        }
        match candidate.rsplit_once(':') {
            Some((prefix, suffix)) if suffix.chars().all(|c| c.is_ascii_digit()) => {
                candidate = prefix;
            }
            _ => return PathBuf::from(&target.maybe_path),
        }
    }
}

fn path_hyperlink_regexes() -> Vec<String> {
    vec![
        // Python-style diagnostics
        "File \"(?<path>[^\"]+)\", line (?<line>[0-9]+)".to_string(),
        [
            "(?x)",
            "[({\\[<]{0,2}",
            "(?<quote>[\"'`])?",
            "(?<link>(?<path>[^ ]+?",
            "    (?<line_column>:+[0-9]+(:[0-9]+)?|:?\\([0-9]+([,:][0-9]+)?\\))?",
            "))",
            "(?(<quote>)\\k<quote>)",
            "[)}\\]>]?",
            "(?(<line_column>):[^ 0-9][^ ]*)?",
            "[.,:)}\\]>]*",
            "([ ]+|$)",
        ]
        .join("\n"),
    ]
}
