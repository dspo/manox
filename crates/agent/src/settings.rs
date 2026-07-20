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
use std::sync::OnceLock;

use crate::collaboration_mode::ModeSettingsMap;
use crate::paths;

/// Process-global snapshot of `[modes.*]` overrides, settled once at `init`
/// from `settings.toml`. Constant across a session, so request-build can read
/// it without re-parsing the file (which would also risk a mid-session flip
/// breaking the provider prefix cache).
static MODES: OnceLock<ModeSettingsMap> = OnceLock::new();

/// Read `settings.toml` once at startup and cache its `[modes.*]` table.
/// Called from `agent::init` after i18n (which reads the same file for the
/// locale). A missing or malformed file yields the empty map — no per-mode
/// overrides, presets alone apply.
pub fn init_modes() {
    let _ = MODES.set(load().modes);
}

/// The cached per-mode override table. Empty before `init_modes` or when the
/// user configured nothing — presets alone then apply.
pub fn modes() -> ModeSettingsMap {
    MODES.get().cloned().unwrap_or_default()
}

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
    /// threshold is a fraction of the model's `max_token_count`; 0.8 leaves a
    /// 20% headroom for the output turn that follows. Compaction also fires
    /// when the estimated request body exceeds 6 MiB. See `compact`.
    #[serde(default)]
    pub auto_compact: AutoCompactSettings,

    /// Per-collaboration-mode overrides (`[modes.plan]` / `[modes.default]`)
    /// layered over the built-in presets: `model`, `reasoning_effort`, and
    /// `developer_instructions`. Resolved at request-build time in
    /// `thread::build_completion_request`.
    #[serde(default, skip_serializing_if = "ModeSettingsMap::is_empty")]
    pub modes: ModeSettingsMap,
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
            threshold: 0.8,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_follow_up_behavior_is_ignored() {
        // Serde ignores unknown fields by default, so existing settings files
        // keep loading after the Queue/Steer preference is removed.
        let raw = r#"
language = "en"
follow_up_behavior = "Steer"
"#;
        let settings: Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.language.as_deref(), Some("en"));
        assert!(
            !toml::to_string(&settings)
                .unwrap()
                .contains("follow_up_behavior")
        );
    }
}
