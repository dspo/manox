//! Input mappings — Keystroke/mouse → terminal byte sequences.
//!
//! - `keys::to_esc_str` — `Keystroke` → ESC sequence (APP_CURSOR / APP_KEYPAD
//!   mode branches).
//! - `mouse` — SGR / normal / utf8 mouse reporting.
//!
//! Color conversion lives in `terminal-ui::theme`; pixel↔grid conversion is
//! handled inline by the render element (it owns cell metrics).

pub mod keys;
pub mod mouse;
