//! Input mappings — Keystroke/mouse → terminal byte sequences.
//!
//! Stage 3 implements:
//! - `keys::to_esc_str` — `Keystroke` → ESC sequence (APP_CURSOR / APP_KEYPAD
//!   mode branches), mirroring zed's `crates/terminal/src/mappings/keys.rs`.
//! - `mouse` — SGR / normal / utf8 mouse reporting.
//! - `colors` — alacritty `Color` → gpui `Hsla` + ANSI-16 theme mapping.
//! - `grid` — pixel ↔ grid coordinate conversion.
