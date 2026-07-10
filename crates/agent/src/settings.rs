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

    /// Auto-compaction: summarize older history into a handoff message when the
    /// live context window fills, so long sessions keep going. The trigger
    /// threshold is a fraction of the model's `max_token_count`; 0.9 leaves a
    /// 10% headroom for the output turn that follows. See `compact`.
    #[serde(default)]
    pub auto_compact: AutoCompactSettings,

    /// How the composer (input area + divider + attachments) is horizontally
    /// aligned in the conversation column. Config-file only; no UI toggle.
    #[serde(default)]
    pub composer_alignment: ComposerAlignment,
}

/// Auto-compaction knobs. Read at the top of each turn loop iteration; a flip
/// takes effect on the next turn.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct AutoCompactSettings {
    pub enabled: bool,
    /// Fraction of `max_token_count` at which a compaction pass fires.
    pub threshold: f64,
}

impl Default for AutoCompactSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 0.9,
        }
    }
}
/// Horizontal alignment of the composer in the conversation column.
///
/// `MainColumn` (default) centers the composer in the full main column — the
/// pre-existing behavior. `MessageList` shifts the composer right by half the
/// outline-rail width so it shares the same center axis as the message
/// content, which sits right of the 40px outline rail.
///
/// Config-file only; no UI toggle. TOML values: `"main-column"`, `"message-list"`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ComposerAlignment {
    MainColumn,
    MessageList,
}

impl Default for ComposerAlignment {
    fn default() -> Self {
        Self::MainColumn
    }
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
