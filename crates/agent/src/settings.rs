//! User settings — `~/.config/cx/manox/settings.toml`.
//!
//! Plain-file preferences. The two language fields are read at startup and on
//! every save: `ui_language` drives the Fluent UI locale (and is swapped live
//! by [`crate::i18n::set_ui_language`]); `agent_language` is snapshotted into
//! each new [`crate::thread::Thread`] and selects that thread's harness / tool
//! description language. `claude_md_excludes` filters which CLAUDE.md
//! instruction files [`crate::claude_md`] loads. Absent file or parse failure
//! is non-fatal: every failure path warns once and yields the default, so a
//! malformed file never blocks startup.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use crate::collaboration_mode::ModeSettingsMap;
use crate::language::Language;
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

/// Build the [`crate::claude_md::LoadContext`] for instruction loading.
///
/// Production reads the real home dir, the platform managed-policy path, and
/// the session-cached `claude_md_excludes`. Test builds are hermetic (an
/// empty context loads nothing user- or machine-level), so thread tests never
/// observe the developer's actual `~/.claude` tree or managed policy.
pub fn claude_md_load_context() -> crate::claude_md::LoadContext {
    #[cfg(test)]
    let ctx = crate::claude_md::LoadContext::default();
    #[cfg(not(test))]
    let ctx = crate::claude_md::LoadContext {
        home: paths::home_dir(),
        managed: crate::claude_md::managed_policy_path(),
        excludes: claude_md_excludes(),
        // External imports stay withheld until the approval flow (a later PR)
        // wires the persisted per-anchor decision in here.
        allow_external: false,
    };
    ctx
}

/// The session-cached `claude_md_excludes` list. Lazily settled on first use
/// so `agent::init` ordering does not matter; a mid-session settings edit
/// takes effect next launch, matching the modes snapshot.
#[cfg(not(test))]
fn claude_md_excludes() -> Vec<String> {
    static EXCLUDES: OnceLock<Vec<String>> = OnceLock::new();
    EXCLUDES.get_or_init(|| load().claude_md_excludes).clone()
}

/// Parsed view of `settings.toml`. Every field is optional so a missing or
/// partial file still yields a usable (defaulted) result.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Settings {
    /// UI locale token (`"en"` / `"zh-CN"`). `None` → English. Drives the
    /// Fluent bundle and is swapped live by [`crate::i18n::set_ui_language`].
    #[serde(default)]
    pub ui_language: Option<String>,

    /// Agent language token (`"en"` / `"zh-CN"`). `None` → follow `ui_language`
    /// at resolve time (see [`Settings::resolve`]). Snapshotted into each new
    /// [`crate::thread::Thread`]; changing it never disturbs existing threads.
    #[serde(default)]
    pub agent_language: Option<String>,

    /// Glob patterns matched against the canonical absolute paths of CLAUDE.md
    /// instruction files; matching files are excluded from the loaded set (the
    /// managed-policy file is exempt). Read once per session via
    /// [`claude_md_load_context`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claude_md_excludes: Vec<String>,

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

    /// Network allowlist for sandboxed bash. Read by
    /// `sandbox::NetworkPolicy::for_project` at registry-build time.
    #[serde(default, skip_serializing_if = "NetworkSettings::is_empty")]
    pub network: NetworkSettings,
}

/// Network allowlist settings for the sandbox. Read once at startup by
/// `NetworkPolicy::for_project`. An empty `allowlist` yields `Blocked`
/// (network fully denied at the seatbelt level); a non-empty list yields
/// `Restricted` (the seatbelt narrows outbound to the local proxy port,
/// and the in-process HTTP proxy enforces the hostname patterns).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct NetworkSettings {
    /// Hostname patterns (exact or `*.subdomain` wildcards) the sandbox
    /// proxy will allow. Examples: `"github.com"`, `"*.npmjs.org"`.
    pub allowlist: Vec<String>,
}

impl NetworkSettings {
    /// Whether the allowlist is empty (no network plumbing needed).
    fn is_empty(&self) -> bool {
        self.allowlist.is_empty()
    }
}

/// Resolved language axes — the canonical [`Language`] values, with a missing
/// `agent_language` falling back to the UI language rather than a hardcoded
/// default, so a user who only ever set `ui_language` gets a coherent
/// same-language agent axis for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedLanguages {
    pub ui: Language,
    pub agent: Language,
}

impl Settings {
    /// Resolve both language axes to canonical [`Language`] values. An unknown
    /// token warns once and resolves to English; a missing `agent_language`
    /// follows `ui` (after `ui`'s own resolution), so the two axes stay aligned
    /// until the user splits them.
    pub fn resolve(&self) -> ResolvedLanguages {
        let ui = self.resolve_axis(self.ui_language.as_deref());
        let agent = self
            .agent_language
            .as_deref()
            .map_or(ui, |tok| self.resolve_axis(Some(tok)));
        ResolvedLanguages { ui, agent }
    }

    /// Resolve a single language token to a [`Language`], warning + falling
    /// back to English on a non-canonical token so a typo never silently
    /// coerces to the wrong locale.
    fn resolve_axis(&self, token: Option<&str>) -> Language {
        match token {
            None => Language::En,
            Some(tok) => match Language::from_token(tok) {
                Some(lang) => lang,
                None => {
                    tracing::warn!(token = tok, "unknown language token; defaulting to English");
                    Language::En
                }
            },
        }
    }
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
    fn unknown_top_level_field_is_ignored() {
        // Serde ignores unknown fields by default, so a stale top-level key left
        // in an old settings.toml never blocks load.
        let raw = r#"
ui_language = "en"
follow_up_behavior = "Steer"
"#;
        let settings: Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.ui_language.as_deref(), Some("en"));
        assert!(
            !toml::to_string(&settings)
                .unwrap()
                .contains("follow_up_behavior")
        );
    }

    #[test]
    fn language_tokens_round_trip() {
        let settings = Settings {
            ui_language: Some("zh-CN".into()),
            agent_language: Some("en".into()),
            ..Default::default()
        };
        let s = toml::to_string_pretty(&settings).unwrap();
        let back: Settings = toml::from_str(&s).unwrap();
        assert_eq!(back.ui_language.as_deref(), Some("zh-CN"));
        assert_eq!(back.agent_language.as_deref(), Some("en"));
    }

    #[test]
    fn old_language_field_does_not_serialize() {
        // The legacy single `language` field is gone; a struct that carries only
        // the new fields must not emit `language =` on serialize.
        let settings = Settings {
            ui_language: Some("en".into()),
            agent_language: Some("zh-CN".into()),
            ..Default::default()
        };
        let s = toml::to_string_pretty(&settings).unwrap();
        assert!(!s.contains("\nlanguage ="));
        assert!(s.contains("ui_language"));
        assert!(s.contains("agent_language"));
    }

    #[test]
    fn agent_language_follows_ui_when_absent() {
        let settings = Settings {
            ui_language: Some("zh-CN".into()),
            agent_language: None,
            ..Default::default()
        };
        let r = settings.resolve();
        assert_eq!(r.ui, Language::ZhCn);
        assert_eq!(r.agent, Language::ZhCn);
    }

    #[test]
    fn agent_language_independent_when_set() {
        let settings = Settings {
            ui_language: Some("zh-CN".into()),
            agent_language: Some("en".into()),
            ..Default::default()
        };
        let r = settings.resolve();
        assert_eq!(r.ui, Language::ZhCn);
        assert_eq!(r.agent, Language::En);
    }

    #[test]
    fn missing_axes_default_to_english() {
        let r = Settings::default().resolve();
        assert_eq!(r.ui, Language::En);
        assert_eq!(r.agent, Language::En);
    }

    #[test]
    fn unknown_token_resolves_to_english() {
        let settings = Settings {
            ui_language: Some("fr".into()),
            agent_language: Some("zh".into()),
            ..Default::default()
        };
        let r = settings.resolve();
        assert_eq!(r.ui, Language::En);
        assert_eq!(r.agent, Language::En);
    }
}
