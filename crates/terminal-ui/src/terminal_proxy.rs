//! Transitional gpui adapter around the gpui-free [`TerminalHandle`].
//!
//! The kernel `Terminal` no longer lives in a gpui `Entity`: its state sits
//! behind a lock inside a `TerminalHandle` and its events flow on a broadcast
//! channel. The element / view layer still reads and mutates through an
//! `Entity<…>` and subscribes with `cx.subscribe`, so this adapter re-wraps
//! the handle as a gpui `Entity`, forwards every accessor and mutation
//! through the handle's lock, and pumps the event channel into `cx.emit`.
//!
//! Transitional adapter, removed in γ when the terminal surface moves to a
//! client store.

use std::sync::Arc;

use gpui::{Context, EventEmitter};
use terminal::TerminalHandle;
use terminal::event::TerminalEvent;

/// A gpui `Entity` owning a gpui-free [`TerminalHandle`], re-emitting the
/// handle's events so `cx.subscribe` keeps working unchanged.
pub struct TerminalProxy {
    handle: TerminalHandle,
    _pump: gpui::Task<()>,
}

impl EventEmitter<TerminalEvent> for TerminalProxy {}

impl TerminalProxy {
    /// Wrap a handle and start pumping its event channel into gpui events.
    pub fn new(handle: TerminalHandle, cx: &mut Context<Self>) -> Self {
        let rx = handle.subscribe();
        let _pump = cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            while let Ok(ev) = rx.recv().await {
                // The broadcast clones one `Arc<TerminalEvent>` per event to
                // each subscriber. With a single subscriber the local `Arc`
                // is dropped at the end of the broadcast iteration, leaving
                // the channel's clone at refcount 1, so `try_unwrap` yields
                // the owned event for `cx.emit` (`TerminalEvent` is not
                // `Clone`). A second subscriber would leave refcount ≥ 2 and
                // the event would be dropped here — the proxy relies on being
                // the sole subscriber to its handle.
                if let Ok(ev) = Arc::try_unwrap(ev) {
                    let _ = this.update(cx, |_, cx| cx.emit(ev));
                } else {
                    tracing::warn!(
                        "TerminalProxy pump dropped event: multiple subscribers \
                         on the same TerminalHandle (assumed sole)"
                    );
                }
            }
        });
        Self { handle, _pump }
    }

    /// The wrapped handle — the escape hatch for the API surface whose
    /// argument types the kernel keeps `pub(crate)` (notably `with_term`).
    pub fn handle(&self) -> &TerminalHandle {
        &self.handle
    }

    // ── Read forwarding ───────────────────────────────────────────────────
    // Each getter mirrors the `Terminal` accessor of the same name and
    // returns an owned clone of the locked state (a reference cannot outlive
    // the `read` closure).

    pub fn id(&self) -> String {
        self.handle.read(|t| t.id.clone())
    }

    pub fn rows(&self) -> usize {
        self.handle.read(|t| t.rows)
    }

    pub fn cols(&self) -> usize {
        self.handle.read(|t| t.cols)
    }

    pub fn cwd(&self) -> std::path::PathBuf {
        self.handle.read(|t| t.cwd.clone())
    }

    pub fn title(&self) -> Option<String> {
        self.handle.read(|t| t.title.clone())
    }

    /// Borrow the alacritty `Term` for the renderable snapshot. The element
    /// cannot name the kernel's `pub(crate)` term type, so this forwards
    /// generically and the caller only touches the trait surface it needs.
    pub fn with_term<R>(
        &self,
        f: impl FnOnce(&terminal::Term<terminal::event::ManoxListener>) -> R,
    ) -> R {
        self.handle.read(|t| t.with_term(f))
    }

    pub fn bell(&self) -> terminal::settings::BellMode {
        self.handle.read(|t| t.bell)
    }

    pub fn is_ready(&self) -> bool {
        self.handle.read(|t| t.is_ready())
    }

    pub fn mode(&self) -> terminal::alacritty_terminal::term::TermMode {
        self.handle.read(|t| t.mode())
    }

    pub fn selection_to_string(&self) -> Option<String> {
        self.handle.read(|t| t.selection_to_string())
    }

    pub fn foreground_process_name(&self) -> Option<String> {
        self.handle.read(|t| t.foreground_process_name())
    }

    pub fn cursor_blinking(&self) -> bool {
        self.handle.read(|t| t.cursor_blinking())
    }

    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<String> {
        self.handle.read(|t| t.hyperlink_at(row, col))
    }

    pub fn hover_target(&self, row: usize, col: usize) -> Option<terminal::HoverTarget> {
        self.handle.read(|t| t.hover_target(row, col))
    }

    pub fn search_matches(
        &self,
        pattern: &str,
    ) -> Result<Vec<(terminal::Point, terminal::Point)>, String> {
        self.handle.read(|t| t.search_matches(pattern))
    }

    // ── Mutation forwarding ───────────────────────────────────────────────
    // `with_mut` takes the write lock and broadcasts the events the mutation
    // buffered, so these mirror the old `entity.update(cx, |t, cx| …)` calls
    // with no `cx` of their own.

    pub fn input(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.handle.with_mut(|t| t.input(bytes))
    }

    pub fn paste(&self, text: &str) -> std::io::Result<()> {
        self.handle.with_mut(|t| t.paste(text))
    }

    pub fn resize(&self, cols: usize, rows: usize) {
        self.handle.with_mut(|t| t.resize(cols, rows));
    }

    pub fn scroll(&self, delta: i32) {
        self.handle.with_mut(|t| t.scroll(delta));
    }

    pub fn scroll_to_fraction(&self, fraction: f32) {
        self.handle.with_mut(|t| t.scroll_to_fraction(fraction));
    }

    pub fn alternate_scroll(&self, delta_lines: i32) {
        self.handle.with_mut(|t| t.alternate_scroll(delta_lines));
    }

    pub fn mouse_wheel(
        &self,
        row: usize,
        col: usize,
        delta_lines: i32,
        modifiers: &terminal::Modifiers,
    ) {
        self.handle
            .with_mut(|t| t.mouse_wheel(row, col, delta_lines, modifiers));
    }

    pub fn start_selection(
        &self,
        ty: terminal::alacritty_terminal::selection::SelectionType,
        row: usize,
        col: usize,
    ) {
        self.handle.with_mut(|t| t.start_selection(ty, row, col));
    }

    pub fn update_selection(&self, row: usize, col: usize) {
        self.handle.with_mut(|t| t.update_selection(row, col));
    }

    pub fn clear_selection(&self) {
        self.handle.with_mut(|t| t.clear_selection());
    }

    pub fn toggle_vi_mode(&self) {
        self.handle.with_mut(|t| t.toggle_vi_mode());
    }

    pub fn vi_motion(&self, motion: terminal::alacritty_terminal::vi_mode::ViMotion) {
        self.handle.with_mut(|t| t.vi_motion(motion));
    }
}
