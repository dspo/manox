//! User settings — `~/.manox/settings.toml`.
//!
//! Plain-file preferences. `claude_md_excludes` filters which CLAUDE.md
//! instruction files [`crate::claude_md`] loads. Absent file or parse failure
//! is non-fatal: every failure path warns once and yields the default, so a
//! malformed file never blocks startup.
//!
//! This repository carries no language configuration: agent-facing prose is
//! English only and UI chrome localization belongs to the host application,
//! which owns the `ui_language` key in the same file independently.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use crate::paths;

static CONTEXT_OPT: OnceLock<ContextOptimizationSettings> = OnceLock::new();
static EDIT: OnceLock<EditSettings> = OnceLock::new();
static SIDE_CALLS: OnceLock<SideCallsSettings> = OnceLock::new();
static MCP_DISABLED: OnceLock<Vec<String>> = OnceLock::new();

/// Cache the optimization and side-call tables at startup.
pub fn init_optimization() {
    let s = load();
    let _ = CONTEXT_OPT.set(s.context_optimization);
    let _ = SIDE_CALLS.set(s.side_calls);
    let _ = MCP_DISABLED.set(s.mcp.disabled.clone());
    let _ = EDIT.set(s.edit);
}

/// Cached context-optimization settings. Defaults when not yet initialized.
pub fn context_optimization() -> ContextOptimizationSettings {
    CONTEXT_OPT.get().copied().unwrap_or_default().effective()
}

/// Cached hashline Edit settings. Defaults when not yet initialized.
pub fn edit() -> EditSettings {
    EDIT.get().copied().unwrap_or_default()
}

/// Cached side-call settings. Defaults when not yet initialized.
pub fn side_calls() -> SideCallsSettings {
    SIDE_CALLS.get().cloned().unwrap_or_default()
}

/// Cached list of disabled MCP server names (settings `[mcp] disabled`).
/// Read by `mcp::init` at startup; empty when not yet initialized.
pub fn mcp_disabled() -> Vec<String> {
    MCP_DISABLED.get().cloned().unwrap_or_default()
}

/// Persist the disabled-MCP-server list to `settings.toml` and refresh the
/// startup cache (the registry itself rebuilds on next launch).
pub fn set_mcp_disabled(names: Vec<String>) -> Result<()> {
    let mut settings = load();
    settings.mcp.disabled = names.clone();
    save(&settings)?;
    let _ = MCP_DISABLED.set(names);
    Ok(())
}

/// Build the [`crate::claude_md::LoadContext`] for instruction loading.
///
/// Production reads the real home dir, the platform managed-policy path, and
/// the session-cached `claude_md_excludes`. Test builds are hermetic (an
/// empty context loads nothing user- or machine-level), so thread tests never
/// observe the developer's actual `~/.claude` tree or managed policy.
/// Parsed view of `settings.toml`. Every field is optional so a missing or
/// partial file still yields a usable (defaulted) result.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Settings {
    /// Default model for new pi-harness threads: a registry model id
    /// (`deepseek-v4-flash`) or a Claude/OpenAI alias (`sonnet`). Unset or
    /// unresolvable → the first registered model (sorted).
    #[serde(default)]
    pub default_model: Option<String>,

    /// Glob patterns matched against the canonical absolute paths of CLAUDE.md
    /// instruction files; matching files are excluded from the loaded set (the
    /// managed-policy file is exempt). Read once per session via
    /// [`claude_md_load_context`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claude_md_excludes: Vec<String>,

    /// Context optimization: tool discovery, history rewrite, pruning, and
    /// compact output knobs. Shadow mode collects metrics without affecting
    /// model-facing content.
    #[serde(default)]
    pub context_optimization: ContextOptimizationSettings,

    /// Per-purpose side-call policies: title generation.
    #[serde(default)]
    pub side_calls: SideCallsSettings,

    /// MCP server switches (settings UI). The registry is built once at
    /// startup, so changes apply on the next launch.
    #[serde(default)]
    pub mcp: McpSettings,

    /// ChromeUse engine settings (executable / headless / profile / attach
    /// endpoint). Read lazily at the first ChromeUse tool call.
    #[serde(default)]
    pub chrome: ChromeSettings,

    /// Hashline Edit tool switches. Read once per harness build, so changes
    /// apply to new threads.
    #[serde(default)]
    pub edit: EditSettings,
}

/// Hashline Edit tool switches. `enforce_seen_lines` opts a host into the
/// anti-blind-edit guard; it ships off (matching upstream oh-my-pi) because
/// the guard trades edit-rejection round-trips for blind-anchor safety.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct EditSettings {
    /// Reject edits whose anchor lines the read that minted the tag never
    /// displayed. Default `false`.
    pub enforce_seen_lines: bool,
}

/// MCP toggles: server names the user switched off in the settings panel.
/// Absent from the list = enabled (connect at startup).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct McpSettings {
    /// Server names skipped by `mcp::init` (persisted by the settings UI).
    pub disabled: Vec<String>,
}

/// ChromeUse session settings for the built-in Chrome automation engine
/// (`chrome_use` module). Every field is optional; absent values resolve to
/// engine defaults at launch time.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ChromeSettings {
    /// Chrome/Chromium executable path. `None` → engine discovery
    /// (`RUSTWRIGHT_CHROMIUM` / `CHROME` / `CHROMIUM` env, then the common
    /// install locations).
    pub executable: Option<String>,
    /// Launch Chrome headless. Default `false`: ChromeUse drives a real,
    /// user-visible Chrome window.
    pub headless: bool,
    /// Chrome user-data (profile) directory. `None` →
    /// `~/.manox/chrome-profile/`, so logins persist across sessions.
    pub user_data_dir: Option<String>,
    /// Attach to an already-running Chrome over its DevTools endpoint
    /// (`ws://127.0.0.1:9222/...`) instead of launching a new process;
    /// keeps the user's existing logins and tabs.
    pub cdp_endpoint: Option<String>,
}

// ── context optimization settings ────────────────────────────────────

/// Feature flags for context optimization. Each dimension is independently
/// toggleable: `off`, `shadow` (collect metrics only, don't alter model-facing
/// content), or `on` (apply the optimization).
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct ContextOptimizationSettings {
    /// Master rollback switch. `false` forces every optimization axis off,
    /// including Code, without discarding the configured per-axis rollout.
    #[serde(default = "default_context_optimization_enabled")]
    pub enabled: bool,
    /// Tool discovery via BM25 `ToolSearch`. When `shadow`, the tool schema
    /// is sent but activations are logged, not applied.
    #[serde(default)]
    pub tool_discovery: Toggle,
    /// Per-turn tool-result compact rewriting.
    #[serde(default)]
    pub history_rewrite: Toggle,
    /// Superseded-read / useless-result pruning with hot-prefix protection.
    #[serde(default)]
    pub history_pruning: Toggle,
    /// Whether tool results use per-tool output budgets (default 50 KiB cap
    /// remains when false).
    #[serde(default)]
    pub compact_outputs: bool,
    /// Code-mode orchestration: `off` or `hybrid` (keep native tools + add
    /// the restricted QuickJS `Code` tool).
    #[serde(default)]
    pub code_mode: CodeModeToggle,
}

impl ContextOptimizationSettings {
    pub fn effective(mut self) -> Self {
        if !self.enabled {
            self.tool_discovery = Toggle::Off;
            self.history_rewrite = Toggle::Off;
            self.history_pruning = Toggle::Off;
            self.compact_outputs = false;
            self.code_mode = CodeModeToggle::Off;
        }
        self
    }

    /// `compact_outputs` is the original boolean rollout knob; preserve it as
    /// an `On` alias so existing configurations are not silently ignored.
    pub fn effective_history_rewrite(self) -> Toggle {
        if self.compact_outputs {
            Toggle::On
        } else {
            self.history_rewrite
        }
    }
}

const fn default_context_optimization_enabled() -> bool {
    true
}

impl Default for ContextOptimizationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            tool_discovery: Toggle::On,
            history_rewrite: Toggle::Shadow,
            history_pruning: Toggle::Shadow,
            compact_outputs: false,
            code_mode: CodeModeToggle::Off,
        }
    }
}

/// Three-state toggle for optimization features.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Toggle {
    #[default]
    Off,
    /// Collect metrics but don't alter model-facing content.
    Shadow,
    /// Apply the optimization.
    On,
}

/// Code-mode toggle: `off` (no `Code` tool) or `hybrid` (add `Code` tool
/// alongside native tools; the model can use either).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeModeToggle {
    #[default]
    Off,
    Hybrid,
}

// ── side-call policy settings ────────────────────────────────────────

/// Per-purpose side-call policies. Each key maps to a [`SideCallPolicy`];
/// absent keys use the defaults below.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SideCallsSettings {
    #[serde(default)]
    pub title: SideCallPolicy,
}

/// A side-call policy: which model, reasoning effort, output cap, and
/// whether the call is enabled at all. An empty `model` reuses the main
/// threadʼs active model. `max_output_tokens = 0` means uncapped.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SideCallPolicy {
    /// Model id override. Empty → inherit the main model.
    #[serde(default)]
    pub model: String,
    /// Reasoning effort: `low`, `medium`, `high`, or `xhigh` (ultracode).
    /// `None` → no thinking (deterministic, fast).
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Output token cap. `0` → no per-request override.
    #[serde(default)]
    pub max_output_tokens: u32,
    /// Whether this side call runs at all.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl SideCallsSettings {
    /// Resolved title policy: user config overlaid on the title preset.
    pub fn title_policy(&self) -> SideCallPolicy {
        resolve_side_call_policy(&self.title, SideCallPolicy::title_default())
    }
}

impl Default for SideCallPolicy {
    fn default() -> Self {
        Self {
            model: String::new(),
            reasoning_effort: None,
            max_output_tokens: 0,
            enabled: true,
        }
    }
}

/// Convert a resolved [`SideCallPolicy`]'s `max_output_tokens` into the
/// `Option<u32>` that [`LanguageModelRequest::max_output_tokens`] expects.
/// `0` (the default "no override" sentinel) maps to `None`.
pub fn side_call_output_cap(policy: SideCallPolicy) -> Option<u32> {
    if policy.max_output_tokens > 0 && policy.enabled {
        Some(policy.max_output_tokens)
    } else {
        None
    }
}

/// Preset defaults for each side-call purpose (used when the user hasn't
/// configured a specific key).
impl SideCallPolicy {
    pub fn title_default() -> Self {
        Self {
            model: String::new(),
            reasoning_effort: Some("low".into()),
            max_output_tokens: 128,
            enabled: true,
        }
    }
}

/// Resolve the configured effort. Empty preset strings are translated to the
/// purpose default here so serde-facing settings stay backwards-compatible.
pub fn side_call_effort(
    policy: &SideCallPolicy,
    default: crate::language_model::RequestReasoningEffort,
) -> Option<crate::language_model::RequestReasoningEffort> {
    let raw = policy.reasoning_effort.as_deref().unwrap_or("").trim();
    let effort = match raw {
        "" => default,
        "low" => crate::language_model::RequestReasoningEffort::Low,
        "medium" => crate::language_model::RequestReasoningEffort::Medium,
        "high" => crate::language_model::RequestReasoningEffort::High,
        "max" | "xhigh" => crate::language_model::RequestReasoningEffort::Max,
        other => {
            tracing::warn!(
                effort = other,
                "unknown side-call reasoning effort; using default"
            );
            default
        }
    };
    Some(effort)
}

/// Resolve a side-call policy: user-configured fields win; empty/zero fields
/// fall back to the per-purpose preset.
pub fn resolve_side_call_policy(user: &SideCallPolicy, preset: SideCallPolicy) -> SideCallPolicy {
    let model = if user.model.is_empty() {
        preset.model
    } else {
        user.model.clone()
    };
    let reasoning_effort = user.reasoning_effort.clone().or(preset.reasoning_effort);
    let max_output_tokens = if user.max_output_tokens == 0 {
        preset.max_output_tokens
    } else {
        user.max_output_tokens
    };
    let enabled = user.enabled;
    SideCallPolicy {
        model,
        reasoning_effort,
        max_output_tokens,
        enabled,
    }
}

/// Load settings from `settings.toml`. Always returns a usable [`Settings`] —
/// every failure (missing path, missing file, parse error) warns once and
/// falls back to the default.
/// Build the [`crate::claude_md::LoadContext`] for instruction loading:
/// excludes come from settings; home + managed policy resolve to platform
/// defaults; external imports stay withheld (no approval surface yet).
pub fn claude_md_load_context() -> crate::claude_md::LoadContext {
    let settings = load();
    crate::claude_md::LoadContext {
        home: crate::paths::home_dir(),
        managed: crate::claude_md::managed_policy_path(),
        excludes: settings.claude_md_excludes.clone(),
        allow_external: false,
    }
}

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

/// Persist `settings` to `settings.toml`, creating the manox config directory
/// on demand.
///
/// This crate shares the file with the host application, which owns keys this
/// struct does not model (currently `ui_language`). A whole-document rewrite
/// would silently delete them, so the write is a parse-edit-merge: the on-disk
/// document is read, the tables this struct owns are replaced wholesale, and
/// every other key is carried over verbatim. Within an owned table a removed
/// field does disappear, which is the intended behavior for our own schema.
///
/// Errors are returned to the caller — settings writes originate from explicit
/// user action (UI save button), so surfacing a failure is the right move.
pub fn save(settings: &Settings) -> Result<()> {
    let dir = paths::ensure_manox_config_dir()
        .context("ensuring manox config dir exists before writing settings.toml")?;
    let path = dir.join("settings.toml");
    let existing = match std::fs::read_to_string(&path) {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("reading settings.toml"),
    };
    let body = merge_into_existing(settings, existing.as_deref())?;
    std::fs::write(&path, body)
        .with_context(|| format!("writing settings.toml at {}", path.display()))?;
    Ok(())
}

/// Render `settings` as a document to write, preserving any keys in
/// `existing` that this crate does not own.
///
/// Split out from [`save`] so the merge semantics are testable without
/// touching the filesystem or the process environment.
fn merge_into_existing(settings: &Settings, existing: Option<&str>) -> Result<String> {
    let owned = toml::Value::try_from(settings)
        .context("serializing settings.toml")?
        .as_table()
        .cloned()
        .unwrap_or_default();

    let mut doc = match existing {
        Some(raw) => toml::from_str::<toml::Value>(raw).unwrap_or_else(|e| {
            // A corrupt file cannot be merged into; replacing it is the only
            // way forward, and the warn keeps the loss diagnosable.
            tracing::warn!(error = %e, "settings.toml parse failed; rewriting it from scratch");
            toml::Value::Table(toml::map::Map::new())
        }),
        None => toml::Value::Table(toml::map::Map::new()),
    };
    let table = doc
        .as_table_mut()
        .context("settings.toml top level is not a table")?;
    for (key, value) in owned {
        table.insert(key, value);
    }
    toml::to_string_pretty(&doc).context("serializing settings.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_this_crate_does_not_own_are_ignored_on_load() {
        // The host application owns keys in the same file (currently
        // `ui_language`), and a stale top-level key may linger from an older
        // build. Neither may block a load: unknown fields are dropped rather
        // than rejected.
        let raw = r#"
ui_language = "zh-CN"
follow_up_behavior = "Steer"
"#;
        let settings: Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.default_model, None);
    }

    /// `settings.toml` is shared with the host application, which owns keys
    /// this crate does not model (`ui_language`). Saving must not delete them:
    /// a whole-document rewrite would, silently resetting the user's chosen UI
    /// language the next time any owned setting changed.
    #[test]
    fn save_preserves_host_owned_keys() {
        let existing = r#"ui_language = "en"
default_model = "kept"
"#;
        let settings = Settings {
            default_model: Some("written".into()),
            mcp: McpSettings {
                disabled: vec!["some-server".into()],
            },
            ..Default::default()
        };

        let out = merge_into_existing(&settings, Some(existing)).unwrap();

        assert!(
            out.contains(r#"ui_language = "en""#),
            "the host-owned key was dropped:\n{out}"
        );
        // Keys this crate owns are written from the struct.
        assert!(out.contains("written"), "owned write missing:\n{out}");
        assert!(out.contains("some-server"), "owned write missing:\n{out}");
    }

    /// With no file on disk the merge still produces the owned schema, so a
    /// first-ever save is complete rather than empty.
    #[test]
    fn save_without_an_existing_file_writes_the_owned_schema() {
        let settings = Settings {
            default_model: Some("written".into()),
            ..Default::default()
        };
        let out = merge_into_existing(&settings, None).unwrap();
        assert!(out.contains("written"), "owned value missing:\n{out}");
        assert!(
            out.contains("[context_optimization]"),
            "schema missing:\n{out}"
        );
    }

    /// A corrupt file cannot be merged into, so it is replaced rather than
    /// blocking a save forever.
    #[test]
    fn save_recovers_from_a_corrupt_existing_file() {
        let settings = Settings {
            default_model: Some("written".into()),
            ..Default::default()
        };
        let out = merge_into_existing(&settings, Some("this is not = valid = toml")).unwrap();
        assert!(out.contains("written"), "owned value missing:\n{out}");
        assert!(
            out.contains("[context_optimization]"),
            "schema missing:\n{out}"
        );
    }

    /// On-disk round trip through the merge: the document written back must
    /// still carry a host-owned `ui_language` and any unowned value, so the app
    /// reads the same language after the runtime saved one of its own settings.
    #[test]
    fn merge_round_trip_keeps_host_owned_keys_on_disk() {
        let before = "ui_language = \"en\"\ndefault_model = \"x\"\n";
        let settings = Settings {
            mcp: McpSettings {
                disabled: vec!["srv".into()],
            },
            ..Default::default()
        };

        let after = merge_into_existing(&settings, Some(before)).unwrap();

        assert!(
            after.contains("ui_language = \"en\""),
            "host-owned key dropped:\n{after}"
        );
        assert!(
            after.contains("default_model = \"x\""),
            "unowned value dropped:\n{after}"
        );
        assert!(after.contains("srv"), "owned write missing:\n{after}");
        // And the app's parser still finds the language it wrote.
        let parsed: toml::Value = toml::from_str(&after).unwrap();
        assert_eq!(
            parsed.get("ui_language").and_then(|v| v.as_str()),
            Some("en")
        );
    }

    // ── context optimization / side-call tests ──────────────────────

    #[test]
    fn context_optimization_defaults_to_discovery_on() {
        let s = ContextOptimizationSettings::default();
        assert!(s.enabled);
        // Tool discovery defaults to On so only core tools appear in the
        // initial schema; the rest are discovered via ToolSearch or activated
        // on first use. History rewrite/pruning stay Shadow (metrics only).
        assert_eq!(s.tool_discovery, Toggle::On);
        assert_eq!(s.history_rewrite, Toggle::Shadow);
        assert_eq!(s.history_pruning, Toggle::Shadow);
        assert!(!s.compact_outputs);
        assert_eq!(s.code_mode, CodeModeToggle::Off);
    }

    #[test]
    fn context_optimization_master_switch_forces_every_axis_off() {
        let effective = ContextOptimizationSettings {
            enabled: false,
            tool_discovery: Toggle::On,
            history_rewrite: Toggle::On,
            history_pruning: Toggle::On,
            compact_outputs: true,
            code_mode: CodeModeToggle::Hybrid,
        }
        .effective();
        assert_eq!(effective.tool_discovery, Toggle::Off);
        assert_eq!(effective.history_rewrite, Toggle::Off);
        assert_eq!(effective.history_pruning, Toggle::Off);
        assert!(!effective.compact_outputs);
        assert_eq!(effective.code_mode, CodeModeToggle::Off);
    }

    #[test]
    fn toggle_serde_round_trips() {
        for (val, json) in [
            (Toggle::Off, "\"off\""),
            (Toggle::Shadow, "\"shadow\""),
            (Toggle::On, "\"on\""),
        ] {
            assert_eq!(serde_json::to_string(&val).unwrap(), json);
            let back: Toggle = serde_json::from_str(json).unwrap();
            assert_eq!(back, val);
        }
    }

    #[test]
    fn code_mode_toggle_serde_round_trips() {
        assert_eq!(
            serde_json::to_string(&CodeModeToggle::Off).unwrap(),
            "\"off\""
        );
        assert_eq!(
            serde_json::to_string(&CodeModeToggle::Hybrid).unwrap(),
            "\"hybrid\""
        );
        let back: CodeModeToggle = serde_json::from_str("\"hybrid\"").unwrap();
        assert_eq!(back, CodeModeToggle::Hybrid);
    }

    #[test]
    fn side_call_policy_defaults() {
        let p = SideCallPolicy::default();
        assert!(p.model.is_empty());
        assert!(p.reasoning_effort.is_none());
        assert_eq!(p.max_output_tokens, 0);
        assert!(p.enabled);
    }

    #[test]
    fn side_call_presets() {
        assert_eq!(SideCallPolicy::title_default().max_output_tokens, 128);
    }

    #[test]
    fn resolve_side_call_policy_user_wins() {
        let user = SideCallPolicy {
            model: "custom".into(),
            reasoning_effort: Some("high".into()),
            max_output_tokens: 500,
            enabled: false,
        };
        let r = resolve_side_call_policy(&user, SideCallPolicy::title_default());
        assert_eq!(r.model, "custom");
        assert_eq!(r.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(r.max_output_tokens, 500);
        assert!(!r.enabled);
    }

    #[test]
    fn resolve_side_call_policy_falls_back() {
        let user = SideCallPolicy::default(); // all empty/zero
        let r = resolve_side_call_policy(&user, SideCallPolicy::title_default());
        assert_eq!(r.max_output_tokens, 128);
        assert!(r.enabled);
        assert!(r.model.is_empty()); // preset's model is also empty
    }

    #[test]
    fn settings_deser_with_context_optimization() {
        let raw = r#"
ui_language = "en"

[context_optimization]
enabled = true
tool_discovery = "shadow"
history_rewrite = "shadow"
history_pruning = "shadow"
compact_outputs = false
code_mode = "off"

[side_calls.title]
max_output_tokens = 128

"#;
        let s: Settings = toml::from_str(raw).unwrap();
        assert_eq!(s.context_optimization.tool_discovery, Toggle::Shadow);
        assert_eq!(s.side_calls.title.max_output_tokens, 128);
    }
}
