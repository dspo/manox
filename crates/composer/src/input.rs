//! The composer's text input view.
//!
//! Stores UTF-8 text with byte-offset cursor and selection, maintains an IME
//! marked range, and emits `ComposerEvent`. Implements `EntityInputHandler` so
//! the platform IME (macOS NSTextInputClient) drives composition through us.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, Render, SharedString, UTF16Selection, Window, div, point, prelude::*, px,
};

use crate::str_util;
use crate::{
    Backspace, BlinkCursor, CONTEXT, Copy, Cut, Delete, End, Enter, Home, Left, Newline, Paste,
    Right, SelectAll, SelectLeft, SelectRight, ShowCharacterPalette, TextElement,
};

/// Events the composer emits to its host. `PastedImage` is the reason this
/// input exists — it carries a clipboard image up to the layer that owns
/// attachments and resize.
#[derive(Clone, Debug)]
pub enum ComposerEvent {
    Change,
    PressEnter,
    PastedImage(gpui::Image),
    Focus,
    Blur,
}

pub struct ComposerInput {
    pub(crate) text: String,
    pub(crate) selected_range: Range<usize>,
    pub(crate) selection_reversed: bool,
    pub(crate) ime_marked_range: Option<Range<usize>>,

    pub(crate) focus_handle: FocusHandle,

    pub(crate) multi_line: bool,
    pub(crate) auto_grow_min_rows: usize,
    pub(crate) auto_grow_max_rows: usize,
    pub(crate) submit_on_enter: bool,
    pub(crate) placeholder: SharedString,

    /// Last shaped layout, written by `TextElement::prepaint` and read by IME
    /// `bounds_for_range` / hit-testing between frames.
    pub(crate) last_lines: Vec<gpui::WrappedLine>,
    pub(crate) last_bounds: Option<Bounds<Pixels>>,
    pub(crate) last_line_height: Pixels,
    pub(crate) computed_rows: usize,

    pub(crate) blink_cursor: Entity<BlinkCursor>,
    pub(crate) is_selecting: bool,
    _subscriptions: Vec<gpui::Subscription>,
}

impl ComposerInput {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let blink_cursor = cx.new(|_| BlinkCursor::new());
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.observe(&blink_cursor, |_, _, cx| cx.notify()));
        subscriptions.push(cx.on_focus(&focus_handle, window, |this, _, cx| {
            this.blink_cursor.update(cx, |b, cx| b.start(cx));
            cx.emit(ComposerEvent::Focus);
        }));
        subscriptions.push(cx.on_blur(&focus_handle, window, |this, _, cx| {
            this.blink_cursor.update(cx, |b, cx| b.stop(cx));
            cx.emit(ComposerEvent::Blur);
        }));

        Self {
            text: String::new(),
            selected_range: 0..0,
            selection_reversed: false,
            ime_marked_range: None,
            focus_handle,
            multi_line: false,
            auto_grow_min_rows: 1,
            auto_grow_max_rows: 1,
            submit_on_enter: false,
            placeholder: "".into(),
            last_lines: Vec::new(),
            last_bounds: None,
            last_line_height: px(0.),
            computed_rows: 0,
            blink_cursor,
            is_selecting: false,
            _subscriptions: subscriptions,
        }
    }

    pub fn multi_line(mut self, yes: bool) -> Self {
        self.multi_line = yes;
        self
    }
    pub fn auto_grow(mut self, min: usize, max: usize) -> Self {
        self.auto_grow_min_rows = min;
        self.auto_grow_max_rows = max;
        self
    }
    pub fn submit_on_enter(mut self, yes: bool) -> Self {
        self.submit_on_enter = yes;
        self
    }
    pub fn placeholder(mut self, text: impl Into<SharedString>) -> Self {
        self.placeholder = text.into();
        self
    }

    pub fn value(&self) -> SharedString {
        self.text.clone().into()
    }

    pub fn set_value(
        &mut self,
        text: impl Into<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text = text.into();
        let end = self.text.len();
        self.selected_range = end..end;
        self.ime_marked_range = None;
        cx.emit(ComposerEvent::Change);
        cx.notify();
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus_handle, cx);
        self.blink_cursor.update(cx, |b, cx| b.start(cx));
    }

    pub(crate) fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.pause_blink(cx);
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn pause_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_cursor.update(cx, |b, cx| b.pause(cx));
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        str_util::previous_boundary(&self.text, offset)
    }
    fn next_boundary(&self, offset: usize) -> usize {
        str_util::next_boundary(&self.text, offset)
    }

    /// Splice `range` to `new_text`, place the caret at the splice tail, clear
    /// any IME mark, and emit `Change`. Single-line mode strips newlines.
    fn replace_text(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        let start = range.start.min(self.text.len());
        let end = range.end.min(self.text.len());
        let new_text = if self.multi_line {
            new_text
        } else {
            &new_text.replace('\n', "")
        };
        self.text.replace_range(start..end, new_text);
        let caret = start + new_text.len();
        self.selected_range = caret..caret;
        self.ime_marked_range = None;
        self.pause_blink(cx);
        cx.emit(ComposerEvent::Change);
        cx.notify();
    }

    /// Pixel position of a byte offset within the last layout, relative to
    /// `origin`. Walks logical lines, each contributing `wrap_boundaries + 1`
    /// visual rows.
    pub(crate) fn point_for_byte(
        &self,
        byte: usize,
        origin: Point<Pixels>,
        lh: Pixels,
    ) -> Option<Point<Pixels>> {
        if self.last_lines.is_empty() {
            return None;
        }
        let mut y = origin.y;
        let mut line_start = 0usize;
        for line in &self.last_lines {
            let len = line.len();
            if byte <= line_start + len {
                let in_line = byte - line_start;
                let p = line.position_for_index(in_line, lh)?;
                return Some(point(origin.x + p.x, y + p.y));
            }
            y += (line.wrap_boundaries().len() + 1) as f32 * lh;
            line_start += len + 1; // +1 for the '\n' that split this logical line
        }
        None
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.text.is_empty() {
            return 0;
        }
        let (Some(bounds), lh) = (self.last_bounds.as_ref(), self.last_line_height) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        let mut y = bounds.top();
        let mut line_start = 0usize;
        for line in &self.last_lines {
            let h = (line.wrap_boundaries().len() + 1) as f32 * lh;
            if position.y >= y && position.y < y + h {
                let local_x = position.x - bounds.left();
                // Approximate: map x on the unwrapped layout, ignoring which
                // visual row the click landed on. Good enough for short prompts.
                let idx = line.unwrapped_layout.closest_index_for_x(local_x);
                return line_start + idx;
            }
            y += h;
            line_start += line.len() + 1;
        }
        self.text.len()
    }

    // --- action handlers ---

    fn backspace(&mut self, _: &Backspace, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                return;
            }
            self.select_to(prev, cx);
        }
        self.replace_text(self.selected_range.clone(), "", cx);
    }

    fn delete(&mut self, _: &Delete, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                return;
            }
            self.select_to(next, cx);
        }
        self.replace_text(self.selected_range.clone(), "", cx);
    }

    fn left(&mut self, _: &Left, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.selected_range.end), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.text.len(), cx);
    }

    fn home(&mut self, _: &Home, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.text.len(), cx);
    }

    fn enter(&mut self, _: &Enter, _window: &mut Window, cx: &mut Context<Self>) {
        // While IME is composing, the platform owns `enter` (it commits); the
        // bound action only fires once composition is done.
        if self.ime_marked_range.is_some() {
            return;
        }
        if self.submit_on_enter {
            cx.emit(ComposerEvent::PressEnter);
        } else if self.multi_line {
            self.replace_text(self.selected_range.clone(), "\n", cx);
        }
    }

    fn newline(&mut self, _: &Newline, _window: &mut Window, cx: &mut Context<Self>) {
        if self.multi_line {
            self.replace_text(self.selected_range.clone(), "\n", cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        let entries = clipboard.entries();
        // If the clipboard carries any image, drop all strings so an alt-text
        // or file-path entry doesn't leak into the composer alongside it.
        let has_image = entries
            .iter()
            .any(|e| matches!(e, gpui::ClipboardEntry::Image(_)));
        let mut pasted_image = false;
        for entry in entries {
            match entry {
                gpui::ClipboardEntry::Image(image) => {
                    cx.stop_propagation();
                    window.prevent_default();
                    pasted_image = true;
                    cx.emit(ComposerEvent::PastedImage(image.clone()));
                }
                gpui::ClipboardEntry::String(s) if !has_image => {
                    self.replace_text(self.selected_range.clone(), &s.text, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
        }
        if pasted_image {
            cx.notify();
        }
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.text[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.text[self.selected_range.clone()].to_string(),
            ));
            self.replace_text(self.selected_range.clone(), "", cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        self.blink_cursor.update(cx, |b, cx| b.start(cx));
        self.is_selecting = true;
        if e.modifiers.shift {
            self.select_to(self.index_for_mouse_position(e.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(e.position), cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(e.position), cx);
        }
    }
}

impl EventEmitter<ComposerEvent> for ComposerInput {}

impl Focusable for ComposerInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ComposerInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("composer-input")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .overflow_hidden()
            .text_size(px(14.))
            .line_height(px(22.))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::newline))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
    }
}

impl EntityInputHandler for ComposerInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let r = str_util::utf16_range_to_byte(&self.text, range_utf16);
        *actual_range = Some(str_util::byte_range_to_utf16(&self.text, r.clone()));
        Some(self.text[r].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: str_util::byte_range_to_utf16(&self.text, self.selected_range.clone()),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.ime_marked_range
            .as_ref()
            .map(|r| str_util::byte_range_to_utf16(&self.text, r.clone()))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.ime_marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| str_util::utf16_range_to_byte(&self.text, r))
            .or(self.ime_marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.replace_text(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| str_util::utf16_range_to_byte(&self.text, r))
            .or(self.ime_marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let start = range.start.min(self.text.len());
        let end = range.end.min(self.text.len());
        let new_text = if self.multi_line {
            new_text.to_string()
        } else {
            new_text.replace('\n', "")
        };
        self.text.replace_range(start..end, &new_text);

        if new_text.is_empty() {
            self.ime_marked_range = None;
        } else {
            self.ime_marked_range = Some(start..start + new_text.len());
        }
        self.selected_range = new_selected_range_utf16
            .map(|r| str_util::utf16_range_to_byte(&self.text, r))
            .map(|nr| start + nr.start..start + nr.end)
            .unwrap_or_else(|| start + new_text.len()..start + new_text.len());

        self.pause_blink(cx);
        cx.emit(ComposerEvent::Change);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if self.last_lines.is_empty() || self.last_line_height == px(0.) {
            return Some(element_bounds);
        }
        let r = str_util::utf16_range_to_byte(&self.text, range_utf16);
        let lh = self.last_line_height;
        let start = self.point_for_byte(r.start, element_bounds.origin, lh)?;
        let end = self.point_for_byte(r.end, element_bounds.origin, lh)?;
        Some(Bounds::from_corners(start, point(end.x, end.y + lh)))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }
}
