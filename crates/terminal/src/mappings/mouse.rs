//! Mouse reporting — SGR / normal / utf8 encodings.
//!
//! Encodes a mouse event only when the terminal is in a mouse mode
//! (`MOUSE_REPORT_CLICK` / `MOUSE_MOTION` / `MOUSE_DRAG`). When no mouse
//! mode is active the caller handles the event locally (selection, scroll).
//!
//! Button codes follow xterm: left=0, middle=1, right=2, release=3,
//! wheel-up=64, wheel-down=65. `col`/`row` are 0-based grid coordinates;
//! the encoded sequence is 1-based.

use alacritty_terminal::term::TermMode;

/// What happened to the mouse. `Motion` is only reported when a mouse
/// motion/drag mode is set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// Encode a mouse event for reporting, or `None` if no mouse mode is on.
pub fn encode(
    button: u8,
    action: MouseAction,
    col: u32,
    row: u32,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }

    // SGR uses the suffix char to mark release, so the button code stays
    // as-is; the legacy encoding reuses code 3 for release.
    let mut code = if mode.contains(TermMode::SGR_MOUSE) {
        button
    } else {
        match action {
            MouseAction::Release => 3,
            _ => button,
        }
    };
    if action == MouseAction::Motion
        && mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG)
    {
        code |= 32;
    }

    if mode.contains(TermMode::SGR_MOUSE) {
        // \x1b[<{code};{col+1};{row+1}M (press) / m (release/motion-up).
        let suffix = if action == MouseAction::Release {
            'm'
        } else {
            'M'
        };
        Some(format!("\x1b[<{};{};{}{}", code, col + 1, row + 1, suffix).into_bytes())
    } else {
        // Legacy: \x1b[M + three bytes, each value+32, clamped to 255.
        let b = code.wrapping_add(32);
        let c = col.saturating_add(33).min(255) as u8;
        let r = row.saturating_add(33).min(255) as u8;
        let mut v = Vec::with_capacity(6);
        v.extend_from_slice(b"\x1b[M");
        v.push(b);
        // UTF-8 mouse would encode values > 95 as UTF-8; we clamp to 255
        // (acceptable for visible terminal sizes) under both modes.
        v.push(c);
        v.push(r);
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_mouse_mode_returns_none() {
        assert!(encode(0, MouseAction::Press, 0, 0, TermMode::NONE).is_none());
    }

    #[test]
    fn sgr_press_left_at_origin() {
        let v = encode(
            0,
            MouseAction::Press,
            0,
            0,
            TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE,
        )
        .unwrap();
        assert_eq!(v, b"\x1b[<0;1;1M");
    }

    #[test]
    fn sgr_release_uses_lowercase_m() {
        let v = encode(
            0,
            MouseAction::Release,
            5,
            2,
            TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE,
        )
        .unwrap();
        assert_eq!(v, b"\x1b[<0;6;3m");
    }

    #[test]
    fn legacy_press_clamps_plus_32() {
        let v = encode(0, MouseAction::Press, 0, 0, TermMode::MOUSE_REPORT_CLICK).unwrap();
        assert_eq!(v, b"\x1b[M\x20\x21\x21");
    }
}
