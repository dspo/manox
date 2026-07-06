//! User settings — `~/.config/cx/manox/settings.toml`.
//!
//! Plain-file preferences read once at startup. The `language` field is
//! consumed by [`crate::i18n`]; `custom_instructions` is consumed by the
//! settings overlay (display + persist round-trip) and may be wired into the
//! agent system prompt in a follow-up. Absent file or parse failure is
//! non-fatal: every failure path warns once and yields the default, so a
//! malformed file never blocks startup.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// Parsed view of `settings.toml`. Every field is optional so a missing or
/// partial file still yields a usable (defaulted) result.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Settings {
    /// UI locale tag, e.g. `"en"` or `"zh-CN"`. `None` → default (English).
    #[serde(default)]
    pub language: Option<String>,

    /// Free-form instructions appended to the agent's system prompt. Persisted
    /// by the settings overlay; not yet injected into the prompt (TODO:
    /// surface via `system_prompt::build_main_system_prompt`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
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

/// Serialize `settings` to `settings.toml`, creating the manox config
/// directory on demand. Errors are returned to the caller — settings writes
/// originate from explicit user action (UI save button), so surfacing a
/// failure is the right move.
pub fn save(settings: &Settings) -> Result<()> {
    let dir = paths::ensure_manox_config_dir()
        .context("ensuring manox config dir exists before writing settings.toml")?;
    let path = dir.join("settings.toml");
    let body = toml::to_string_pretty(settings).context("serializing settings.toml")?;
    std::fs::write(&path, body)
        .with_context(|| format!("writing settings.toml at {}", path.display()))?;
    Ok(())
}
