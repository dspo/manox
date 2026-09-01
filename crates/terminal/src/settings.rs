//! `TerminalSettings` — the `[terminal]` section of `settings.toml`.
//!
//! Read from the same `settings.toml` `manox_agent::settings` parses; the
//! `[terminal]` table is an unknown field to `manox_agent::Settings` (which does not
//! set `deny_unknown_fields`), so the two readers coexist on one file.
//! Absent file / section / parse failure is non-fatal: defaults are returned.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use manox_agent::paths;

/// Cursor glyph drawn at the active cell.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorShapeSetting {
    #[default]
    Block,
    Underline,
    Beam,
}

/// What happens when a program rings the terminal bell.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BellMode {
    /// Silent — the bell event crosses the boundary but nothing is rendered.
    Off,
    /// System alert sound (default).
    #[default]
    System,
    /// Brief background flash on the terminal view.
    Visual,
}

/// Cursor blink policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorBlinkSetting {
    /// Never blink, whatever the program asks.
    Off,
    /// Always blink.
    On,
    /// Blink only while the program asks for it (DECSET 12 / DECSCUSR).
    #[default]
    Terminal,
}

/// OSC 52 (clipboard) access policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Osc52Access {
    /// Honor all OSC 52 store/load requests.
    #[default]
    Allow,
    /// Drop every OSC 52 request.
    Deny,
}

/// Parsed `[terminal]` table. Every field defaults so a partial or absent
/// section still yields a usable configuration. `Default` is implemented
/// manually to mirror the serde `default = …` functions (derive-`Default`
/// would yield empty strings / zeros instead of "Menlo" / 14.0 / …).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TerminalSettings {
    /// Override shell program (`"/bin/zsh"`, `"vim"`…). `None` = the default
    /// user shell resolved by portable-pty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(
        default = "default_font_family",
        skip_serializing_if = "is_default_font_family"
    )]
    pub font_family: String,
    #[serde(
        default = "default_font_size",
        skip_serializing_if = "is_default_font_size"
    )]
    pub font_size: f32,
    #[serde(
        default = "default_line_height",
        skip_serializing_if = "is_default_line_height"
    )]
    pub line_height: f32,
    /// Color theme: a `.ottytheme` name under `~/.manox/themes/`
    /// (or a path to a theme file). `None` derives the palette from the app
    /// theme instead — see `terminal::theme::resolve`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(
        default = "default_scrolling_history",
        skip_serializing_if = "is_default_scrolling_history"
    )]
    pub scrolling_history: usize,
    #[serde(default, skip_serializing_if = "is_default_cursor")]
    pub cursor_shape: CursorShapeSetting,
    #[serde(default, skip_serializing_if = "is_default_cursor_blink")]
    pub cursor_blink: CursorBlinkSetting,
    #[serde(default, skip_serializing_if = "is_default_bell")]
    pub bell: BellMode,
    #[serde(default, skip_serializing_if = "is_default_osc52")]
    pub osc52_access: Osc52Access,
    /// Extra env overrides passed to the child shell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// Keystrokes (gpui syntax) that skip the terminal and keep workbench
    /// precedence while the terminal is focused — the terminal's keystroke
    /// interceptor owns every other key and translates it to the PTY. Same
    /// positive semantics as VS Code's
    /// `terminal.integrated.commandsToSkipShell`; Zed solves the same conflict
    /// statically via per-key `Terminal`-context override bindings, here the
    /// conflict set is configurable. Defaults preserve the app's global
    /// shortcuts.
    #[serde(
        default = "default_commands_to_skip_shell",
        skip_serializing_if = "is_default_commands_to_skip_shell"
    )]
    pub commands_to_skip_shell: Vec<String>,
    #[serde(
        default = "default_block_char_render",
        skip_serializing_if = "is_default_block_char_render"
    )]
    pub block_char_render: bool,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            shell: None,
            font_family: default_font_family(),
            font_size: default_font_size(),
            line_height: default_line_height(),
            theme: None,
            scrolling_history: default_scrolling_history(),
            cursor_shape: CursorShapeSetting::Block,
            cursor_blink: CursorBlinkSetting::Terminal,
            bell: BellMode::System,
            osc52_access: Osc52Access::Allow,
            env: Vec::new(),
            commands_to_skip_shell: default_commands_to_skip_shell(),
            block_char_render: true,
        }
    }
}

fn default_font_family() -> String {
    "Menlo".into()
}
fn is_default_font_family(s: &str) -> bool {
    s == "Menlo"
}
fn default_font_size() -> f32 {
    14.0
}
fn is_default_font_size(f: &f32) -> bool {
    *f == 14.0
}
fn default_line_height() -> f32 {
    1.2
}
fn is_default_line_height(f: &f32) -> bool {
    *f == 1.2
}
fn default_scrolling_history() -> usize {
    10_000
}
fn is_default_scrolling_history(n: &usize) -> bool {
    *n == 10_000
}
fn is_default_cursor(c: &CursorShapeSetting) -> bool {
    matches!(c, CursorShapeSetting::Block)
}
fn is_default_cursor_blink(c: &CursorBlinkSetting) -> bool {
    matches!(c, CursorBlinkSetting::Terminal)
}
fn is_default_bell(b: &BellMode) -> bool {
    matches!(b, BellMode::System)
}
fn is_default_osc52(a: &Osc52Access) -> bool {
    matches!(a, Osc52Access::Allow)
}
fn default_commands_to_skip_shell() -> Vec<String> {
    let mut keys = vec![
        "f11".to_string(),
        "ctrl-g".to_string(),
        "ctrl-b".to_string(),
        "ctrl-shift-c".to_string(),
        "cmd-m".to_string(),
        "ctrl-m".to_string(),
    ];
    #[cfg(target_os = "macos")]
    keys.extend(
        [
            "cmd-q",
            "cmd-ctrl-f",
            "cmd-w",
            "cmd-shift-p",
            "cmd-,",
            "cmd-t",
            "cmd-shift-t",
            "cmd-b",
            "cmd-shift-b",
            "cmd-alt-/",
            "cmd-shift-m",
            "cmd-;",
            "cmd-shift-;",
        ]
        .map(str::to_string),
    );
    #[cfg(not(target_os = "macos"))]
    keys.extend(
        [
            "alt-f4",
            "ctrl-w",
            "ctrl-shift-p",
            "ctrl-,",
            "ctrl-t",
            "ctrl-shift-t",
            "ctrl-alt-b",
            "ctrl-shift-b",
            "ctrl-alt-/",
            "ctrl-shift-m",
            "ctrl-;",
            "ctrl-shift-;",
        ]
        .map(str::to_string),
    );
    keys
}
fn is_default_commands_to_skip_shell(v: &Vec<String>) -> bool {
    *v == default_commands_to_skip_shell()
}

fn default_block_char_render() -> bool {
    true
}
fn is_default_block_char_render(b: &bool) -> bool {
    *b
}
/// Wrapper for parsing only the `[terminal]` table from the whole file.
#[derive(Debug, Default, Deserialize)]
struct Root {
    #[serde(default)]
    terminal: TerminalSettings,
}

/// Load `[terminal]` from `settings.toml`. Always returns a usable result —
/// missing path / file / section / parse failure all fall back to defaults.
pub fn load() -> TerminalSettings {
    let Ok(path) = paths::settings_file() else {
        return TerminalSettings::default();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TerminalSettings::default(),
        Err(e) => {
            tracing::warn!(error = %e, "settings.toml read failed; terminal settings default");
            return TerminalSettings::default();
        }
    };
    match toml::from_str::<Root>(&raw) {
        Ok(r) => r.terminal,
        Err(e) => {
            tracing::warn!(error = %e, "settings.toml parse failed; terminal settings default");
            TerminalSettings::default()
        }
    }
}

/// Persist the `[terminal]` table back into `settings.toml`, preserving other
/// top-level keys. Errors propagate — writes are explicit user actions.
pub fn save(settings: &TerminalSettings) -> Result<()> {
    let path = paths::settings_file()?;
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml::Table = toml::from_str(&raw).unwrap_or_default();
    let terminal_val: toml::Value =
        toml::from_str(&toml::to_string(settings)?).map_err(|e| anyhow::anyhow!(e))?;
    doc.insert("terminal".into(), terminal_val);
    let serialized = toml::to_string_pretty(&doc)?;
    std::fs::write(&path, serialized)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_hardcoded() {
        let s = TerminalSettings::default();
        assert_eq!(s.font_family, "Menlo");
        assert_eq!(s.font_size, 14.0);
        assert_eq!(s.line_height, 1.2);
        assert!(s.theme.is_none());
        assert_eq!(s.scrolling_history, 10_000);
        assert!(matches!(s.cursor_shape, CursorShapeSetting::Block));
        assert!(matches!(s.cursor_blink, CursorBlinkSetting::Terminal));
        assert!(matches!(s.bell, BellMode::System));
        assert!(matches!(s.osc52_access, Osc52Access::Allow));
        assert!(s.env.is_empty());
        assert!(s.shell.is_none());
        assert_eq!(s.commands_to_skip_shell, default_commands_to_skip_shell());
        assert!(s.block_char_render);
    }

    #[test]
    fn parses_terminal_table() {
        let raw = r#"
language = "zh-CN"

[terminal]
font_family = "JetBrains Mono"
font_size = 13.0
theme = "paper"
scrolling_history = 5000
cursor_shape = "beam"
cursor_blink = "off"
bell = "visual"
osc52_access = "deny"
commands_to_skip_shell = ["ctrl-g"]
block_char_render = false
"#;
        let root: Root = toml::from_str(raw).unwrap();
        let s = root.terminal;
        assert_eq!(s.font_family, "JetBrains Mono");
        assert_eq!(s.font_size, 13.0);
        assert_eq!(s.theme.as_deref(), Some("paper"));
        assert_eq!(s.scrolling_history, 5000);
        assert!(matches!(s.cursor_shape, CursorShapeSetting::Beam));
        assert!(matches!(s.cursor_blink, CursorBlinkSetting::Off));
        assert!(matches!(s.bell, BellMode::Visual));
        assert!(matches!(s.osc52_access, Osc52Access::Deny));
        assert_eq!(s.commands_to_skip_shell, vec!["ctrl-g".to_string()]);
        assert!(!s.block_char_render);
    }
}
