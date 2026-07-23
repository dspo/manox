//! `Keystroke` → terminal byte sequence, with `APP_CURSOR` / `APP_KEYPAD`
//! mode branches. Hand-written for the manox subset.
//!
//! Coverage: enter/backspace/tab/escape/space, arrows + home/end with
//! xterm modifier suffixes, page up/down/insert/delete, F1–F12, and the
//! printable ASCII range (ctrl-a..z, alt-prefix, shift-uppercase). The
//! kitty keyboard protocol flags are intentionally not honored (rare in
//! TUI agents).

use gpui::Keystroke;
use rmux_core::input::mode;

/// xterm modifier code: `1 + shift + 2*alt + 4*control`. `1` means none.
fn modifier_code(k: &Keystroke) -> u8 {
    let mut m = 1u8;
    if k.modifiers.shift {
        m += 1;
    }
    if k.modifiers.alt {
        m += 2;
    }
    if k.modifiers.control {
        m += 4;
    }
    m
}

fn has_modifier(k: &Keystroke) -> bool {
    k.modifiers.shift || k.modifiers.alt || k.modifiers.control
}

/// Translate a gpui keystroke into the byte sequence the PTY expects, or
/// `None` if the keystroke produces no input (bare modifier press, unknown
/// key). `mode` is the rmux-core mode flag bitmask.
pub fn to_esc_str(k: &Keystroke, mode_flags: u32) -> Option<String> {
    if k.key.is_empty() {
        return None;
    }
    let app_cursor = mode_flags & mode::MODE_KCURSOR != 0;
    let mod_code = modifier_code(k);
    let has_mod = has_modifier(k);

    // Single-char control keys — modifiers on these are not standard.
    match k.key.as_ref() {
        "enter" | "return" => return Some("\r".into()),
        "backspace" => return Some(if k.modifiers.control { "\x08" } else { "\x7f" }.into()),
        "tab" => {
            return Some(if k.modifiers.shift {
                "\x1b[Z".into()
            } else {
                "\t".into()
            });
        }
        "escape" => return Some("\x1b".into()),
        "space" => {
            return Some(if k.modifiers.control {
                "\x00".into()
            } else {
                " ".into()
            });
        }
        _ => {}
    }

    // Arrow + home/end cluster: \x1b[{L} (normal) / \x1bO{L} (app cursor),
    // or \x1b[1;{mod}{L} when a modifier is held.
    let arrow_home_end = match k.key.as_ref() {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    };
    if let Some(letter) = arrow_home_end {
        return Some(if has_mod {
            format!("\x1b[1;{}{}", mod_code, letter)
        } else if app_cursor {
            format!("\x1bO{}", letter)
        } else {
            format!("\x1b[{}", letter)
        });
    }

    // Tilde cluster (page up/down, insert, delete).
    let tilde = match k.key.as_ref() {
        "pageup" => Some('5'),
        "pagedown" => Some('6'),
        "delete" => Some('3'),
        "insert" => Some('2'),
        _ => None,
    };
    if let Some(n) = tilde {
        return Some(if has_mod {
            format!("\x1b[{};{}~", n, mod_code)
        } else {
            format!("\x1b[{}~", n)
        });
    }

    // F1–F12. F1–F4 use \x1bO{P..S} (or \x1b[1;{mod}{L}); F5–F12 use ~-seqs.
    let fkey = match k.key.as_ref() {
        "f1" => Some((11u8, "P")),
        "f2" => Some((12, "Q")),
        "f3" => Some((13, "R")),
        "f4" => Some((14, "S")),
        "f5" => Some((15, "~")),
        "f6" => Some((17, "~")),
        "f7" => Some((18, "~")),
        "f8" => Some((19, "~")),
        "f9" => Some((20, "~")),
        "f10" => Some((21, "~")),
        "f11" => Some((23, "~")),
        "f12" => Some((24, "~")),
        _ => None,
    };
    if let Some((n, suffix)) = fkey {
        if suffix == "~" {
            return Some(if has_mod {
                format!("\x1b[{};{}~", n, mod_code)
            } else {
                format!("\x1b[{}~", n)
            });
        }
        return Some(if has_mod {
            format!("\x1b[1;{}{}", mod_code, suffix)
        } else {
            format!("\x1bO{}", suffix)
        });
    }

    // Printable ASCII. A single-char key only; multi-char names fall through.
    let mut chars = k.key.chars();
    let first = match chars.next() {
        Some(c) if chars.next().is_none() => c,
        _ => return None,
    };
    if !first.is_ascii() {
        // CJK etc. — stage 6 routes through IME; pass the raw codepoint.
        return Some(first.to_string());
    }

    let c = if k.modifiers.control {
        let lower = first.to_ascii_lowercase();
        if lower.is_ascii_lowercase() {
            ((lower as u8) - b'a' + 1) as char
        } else if first == ' ' {
            '\0'
        } else {
            return None;
        }
    } else if k.modifiers.shift && first.is_ascii_alphabetic() {
        first.to_ascii_uppercase()
    } else {
        first
    };

    let mut s = String::new();
    if k.modifiers.alt {
        s.push('\x1b');
    }
    s.push(c);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};

    fn ks(key: &str, mods: Modifiers) -> Keystroke {
        Keystroke {
            key: key.into(),
            modifiers: mods,
            ..Default::default()
        }
    }

    #[test]
    fn arrow_normal_mode() {
        let s = to_esc_str(&ks("up", Modifiers::default()), 0);
        assert_eq!(s.as_deref(), Some("\x1b[A"));
    }

    #[test]
    fn arrow_app_cursor_mode() {
        let s = to_esc_str(&ks("up", Modifiers::default()), mode::MODE_KCURSOR);
        assert_eq!(s.as_deref(), Some("\x1bOA"));
    }

    #[test]
    fn arrow_with_shift_keeps_normal_seq_with_modifier() {
        let mods = Modifiers {
            shift: true,
            ..Default::default()
        };
        let s = to_esc_str(&ks("up", mods), mode::MODE_KCURSOR);
        // Modifier overrides app-cursor: \x1b[1;2A (2 = shift).
        assert_eq!(s.as_deref(), Some("\x1b[1;2A"));
    }

    #[test]
    fn ctrl_c_yields_etx() {
        let mods = Modifiers {
            control: true,
            ..Default::default()
        };
        let s = to_esc_str(&ks("c", mods), 0);
        assert_eq!(s.as_deref(), Some("\x03"));
    }

    #[test]
    fn alt_prefixes_esc() {
        let mods = Modifiers {
            alt: true,
            ..Default::default()
        };
        let s = to_esc_str(&ks("a", mods), 0);
        assert_eq!(s.as_deref(), Some("\x1ba"));
    }

    #[test]
    fn tab_writes_horizontal_tab() {
        let s = to_esc_str(&ks("tab", Modifiers::default()), 0);
        assert_eq!(s.as_deref(), Some("\t"));
    }

    #[test]
    fn shift_tab_writes_csi_z() {
        let mods = Modifiers {
            shift: true,
            ..Default::default()
        };
        let s = to_esc_str(&ks("tab", mods), 0);
        assert_eq!(s.as_deref(), Some("\x1b[Z"));
    }
}
