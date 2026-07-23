//! Mouse reporting — SGR / normal / utf8 encodings.
//!
//! Encodes a mouse event only when the terminal is in a mouse mode
//! (`MOUSE_STANDARD` / `MOUSE_BUTTON` / `MOUSE_ALL`). When no mouse
//! mode is active the caller handles the event locally (selection, scroll).
//!
//! Button codes follow xterm: left=0, middle=1, right=2, release=3,
//! wheel-up=64, wheel-down=65. `col`/`row` are 0-based grid coordinates;
//! the encoded sequence is 1-based.

use rmux_core::input::mode;

/// What happened to the mouse. `Motion` is only reported when a mouse
/// motion/drag mode is set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// Encode a mouse event for reporting, or `None` if no mouse mode is on.
/// `mode_flags` is the rmux-core mode flag bitmask.
pub fn encode(
    button: u8,
    action: MouseAction,
    col: u32,
    row: u32,
    mode_flags: u32,
) -> Option<Vec<u8>> {
    let mouse_modes = mode::ALL_MOUSE_MODES;
    if mode_flags & mouse_modes == 0 {
        return None;
    }

    // SGR uses the suffix char to mark release, so the button code stays
    // as-is; the legacy encoding reuses code 3 for release.
    let mut code = if mode_flags & mode::MODE_MOUSE_SGR != 0 {
        button
    } else {
        match action {
            MouseAction::Release => 3,
            _ => button,
        }
    };
    // Button-event (1002) reports motion only while a button is held;
    // any-event (1003) reports all motion. Both set the 32-bit motion flag.
    if action == MouseAction::Motion
        && (mode_flags & (mode::MODE_MOUSE_BUTTON | mode::MODE_MOUSE_ALL) != 0)
    {
        code |= 32;
    }

    if mode_flags & mode::MODE_MOUSE_SGR != 0 {
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
        assert!(encode(0, MouseAction::Press, 0, 0, 0).is_none());
    }

    #[test]
    fn sgr_press_left_at_origin() {
        let v = encode(
            0,
            MouseAction::Press,
            0,
            0,
            mode::MODE_MOUSE_STANDARD | mode::MODE_MOUSE_SGR,
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
            mode::MODE_MOUSE_STANDARD | mode::MODE_MOUSE_SGR,
        )
        .unwrap();
        assert_eq!(v, b"\x1b[<0;6;3m");
    }

    #[test]
    fn legacy_press_clamps_plus_32() {
        let v = encode(0, MouseAction::Press, 0, 0, mode::MODE_MOUSE_STANDARD).unwrap();
        assert_eq!(v, b"\x1b[M\x20\x21\x21");
    }
}
