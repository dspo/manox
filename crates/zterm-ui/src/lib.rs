//! A reusable GPUI terminal component.
//!
//! Provides [`TerminalView`], a self-contained, embeddable terminal widget
//! (rendering, IME, search, blink, hover, vi) built on the [`terminal`] core.
//! Host applications create a [`zterm_core::Terminal`] (e.g. via
//! [`TerminalView::new`]) and embed the view with `.child(view)`.

mod element;
mod view;

pub use zterm_core::Terminal;
pub use view::TerminalView;
