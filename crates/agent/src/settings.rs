//! User settings — `~/.config/cx/manox/settings.toml`.
//!
//! Plain-file preferences read once at startup. Today only `language` is
//! consumed (by [`crate::i18n`]); the file is the natural home for future
//! user-tunable knobs that don't belong in the provider config. Absent file or
//! parse failure is non-fatal: every failure path warns once and yields the
//! default, so a malformed file never blocks startup.

use serde::Deserialize;

use crate::paths;

/// Parsed view of `settings.toml`. Every field is optional so a missing or
/// partial file still yields a usable (defaulted) result.
#[derive(Debug, Default, Deserialize)]
pub struct Settings {
    /// UI locale tag, e.g. `"en"` or `"zh-CN"`. `None` → default (English).
    #[serde(default)]
    pub language: Option<String>,
}

/// Load settings from `settings.toml`. Always returns a usable [`Settings`] —
/// every failure (missing path, missing file, parse error) warns once and
/// falls back to the default.
pub fn load() -> Settings {
    let Ok(path) = paths::settings_file() else {
        tracing::warn!("settings.toml path unavailable; using defaults");
        return Settings::default();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
        Err(e) => {
            tracing::warn!(error = %e, "settings.toml read failed; using defaults");
            return Settings::default();
        }
    };
    match toml::from_str::<Settings>(&raw) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "settings.toml parse failed; using defaults");
            Settings::default()
        }
    }
}
