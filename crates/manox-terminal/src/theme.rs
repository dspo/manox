//! Terminal color theme files — the `.ottytheme` TOML format.
//!
//! An `.ottytheme` file carries a `[meta]` header (name/mode/…) and a
//! `[terminal]` section with `foreground`, `background` and a 16-entry
//! `palette` of `#RRGGBB` hex colors. `TerminalSettings.theme` names one of
//! these themes; [`resolve`] finds the file (bare name under the manox
//! themes dir, or an explicit path) and [`parse`] turns it into a
//! [`ThemeFile`] the rendering layer can convert to paintable colors.

use std::path::{Path, PathBuf};

use alacritty_terminal::vte::ansi::Rgb;
use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

/// Parsed `[terminal]` section of an `.ottytheme` file — everything the
/// renderer needs to paint a themed terminal.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeFile {
    /// Theme display name from `[meta].name`, falling back to the file stem.
    pub name: String,
    pub foreground: Rgb,
    pub background: Rgb,
    /// ANSI colors 0..16 (Black, Red, …, BrightWhite) in declaration order.
    pub palette: [Rgb; 16],
}

#[derive(Deserialize)]
struct FileTables {
    #[serde(default)]
    meta: MetaTable,
    terminal: TerminalTable,
}

#[derive(Default, Deserialize)]
struct MetaTable {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct TerminalTable {
    foreground: String,
    background: String,
    palette: Vec<String>,
}

/// Resolve the `[terminal].theme` setting value to a parsed theme.
///
/// A bare name (`"paper"`) looks up `<themes_dir>/paper.ottytheme`; a value
/// containing a path separator or ending in `.ottytheme` is used as a
/// literal path.
pub fn resolve(spec: &str) -> Result<ThemeFile> {
    let path = if spec.contains('/') || spec.ends_with(".ottytheme") {
        PathBuf::from(spec)
    } else {
        manox_agent::paths::themes_dir()?.join(format!("{spec}.ottytheme"))
    };
    load(&path)
}

/// Read and parse one `.ottytheme` file.
pub fn load(path: &Path) -> Result<ThemeFile> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read terminal theme {}", path.display()))?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    parse(&raw, &stem).with_context(|| format!("parse terminal theme {}", path.display()))
}

/// Parse `.ottytheme` TOML text. `fallback_name` stands in when `[meta]`
/// does not name the theme.
pub fn parse(raw: &str, fallback_name: &str) -> Result<ThemeFile> {
    let file: FileTables = toml::from_str(raw)?;
    let terminal = file.terminal;
    let foreground = hex_rgb(&terminal.foreground).context("[terminal].foreground")?;
    let background = hex_rgb(&terminal.background).context("[terminal].background")?;
    let palette: Vec<Rgb> = terminal
        .palette
        .iter()
        .enumerate()
        .map(|(i, entry)| hex_rgb(entry).with_context(|| format!("[terminal].palette[{i}]")))
        .collect::<Result<_>>()?;
    let palette: [Rgb; 16] = palette
        .try_into()
        .map_err(|v: Vec<Rgb>| anyhow::anyhow!("palette needs 16 colors, got {}", v.len()))?;
    Ok(ThemeFile {
        name: file.meta.name.unwrap_or_else(|| fallback_name.to_string()),
        foreground,
        background,
        palette,
    })
}

/// Parse `#RRGGBB` (or bare `RRGGBB`) hex into an `Rgb`.
fn hex_rgb(value: &str) -> Result<Rgb> {
    let digits = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("expected #RRGGBB hex color, got {value:?}");
    }
    let channel = |byte_offset: usize| -> Result<u8> {
        u8::from_str_radix(&digits[byte_offset..byte_offset + 2], 16)
            .map_err(|e| anyhow::anyhow!("{e}"))
    };
    Ok(Rgb {
        r: channel(0)?,
        g: channel(2)?,
        b: channel(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAPER: &str = r##"
[meta]
name = "Paper"
mode = "light"

[terminal]
foreground = "#1A1A1A"
background = "#FCFBF9"
palette = [
    "#1A1A1A", "#A33A3A", "#2B5A38", "#A85A20",
    "#4A7A8A", "#4A3A6A", "#3A7A6A", "#C1BEB5",
    "#8C8A80", "#C36A6A", "#6B9A78", "#C88A50",
    "#7A9AAA", "#8A7A9A", "#6ABAAA", "#EBEBE6",
]
"##;

    #[test]
    fn parses_paper_theme() {
        let theme = parse(PAPER, "paper").unwrap();
        assert_eq!(theme.name, "Paper");
        assert_eq!(
            theme.foreground,
            Rgb {
                r: 0x1A,
                g: 0x1A,
                b: 0x1A
            }
        );
        assert_eq!(
            theme.background,
            Rgb {
                r: 0xFC,
                g: 0xFB,
                b: 0xF9
            }
        );
        assert_eq!(
            theme.palette[1],
            Rgb {
                r: 0xA3,
                g: 0x3A,
                b: 0x3A
            }
        );
        assert_eq!(
            theme.palette[15],
            Rgb {
                r: 0xEB,
                g: 0xEB,
                b: 0xE6
            }
        );
    }

    #[test]
    fn meta_name_falls_back_to_stem() {
        let raw = r##"
[terminal]
foreground = "#000000"
background = "#FFFFFF"
palette = [
    "#000000", "#000000", "#000000", "#000000",
    "#000000", "#000000", "#000000", "#000000",
    "#000000", "#000000", "#000000", "#000000",
    "#000000", "#000000", "#000000", "#000000",
]
"##;
        assert_eq!(parse(raw, "mono").unwrap().name, "mono");
    }

    #[test]
    fn rejects_short_palette() {
        let raw = r##"
[terminal]
foreground = "#000000"
background = "#FFFFFF"
palette = ["#000000"]
"##;
        assert!(parse(raw, "bad").unwrap_err().to_string().contains("16"));
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(hex_rgb("#GGGGGG").is_err());
        assert!(hex_rgb("#12345").is_err());
        assert_eq!(
            hex_rgb("1A2B3C").unwrap(),
            Rgb {
                r: 0x1A,
                g: 0x2B,
                b: 0x3C
            }
        );
    }

    #[test]
    fn loads_theme_file_from_disk() {
        let dir = std::env::temp_dir().join(format!("manox-theme-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paper.ottytheme");
        std::fs::write(&path, PAPER).unwrap();
        let theme = load(&path).unwrap();
        assert_eq!(theme.name, "Paper");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
