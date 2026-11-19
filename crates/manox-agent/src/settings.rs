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
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use crate::paths;

static CONTEXT_OPT: OnceLock<ContextOptimizationSettings> = OnceLock::new();
static EDIT: OnceLock<EditSettings> = OnceLock::new();
/// Unlike the other tables, this one is **mutable at runtime**: the AHP
/// enablement toggle writes it after every decision, so a read-modify-write
/// cycle must see the previous toggle, not the startup snapshot. (A
/// `OnceLock` here silently dropped every write after init — the second
/// toggle in a session resurrected the first one on next launch.)
static MCP_DISABLED: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Cache the optimization tables at startup.
pub fn init_optimization() {
    let s = load();
    let _ = CONTEXT_OPT.set(s.context_optimization);
    *MCP_DISABLED.write() = s.mcp.disabled.clone();
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

/// Cached list of disabled MCP server names (settings `[mcp] disabled`).
/// Read by `mcp::init` at startup; empty when not yet initialized.
pub fn mcp_disabled() -> Vec<String> {
    MCP_DISABLED.read().clone()
}

/// Persist the disabled-MCP-server list to `settings.toml` and refresh the
/// startup cache (the registry itself rebuilds on next launch).
pub fn set_mcp_disabled(names: Vec<String>) -> Result<()> {
    let mut settings = load();
    settings.mcp.disabled = names.clone();
    save(&settings)?;
    *MCP_DISABLED.write() = names;
    Ok(())
}

/// Atomically apply one mutation to the disabled-MCP list: the read-modify-
/// write (file + cache) is serialized behind a process-wide lock, so two
/// concurrent toggles can no longer drop each other's entry.
pub fn modify_mcp_disabled(f: impl FnOnce(Vec<String>) -> Vec<String>) -> Result<Vec<String>> {
    static MODIFY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = MODIFY_LOCK.lock();
    let next = f(mcp_disabled());
    set_mcp_disabled(next.clone())?;
    Ok(next)
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
    /// Default model for new threads: a registry model id
    /// (`deepseek-v4-flash`) or a Claude/OpenAI alias (`sonnet`). Unset or
    /// unresolvable → the first registered model (sorted).
    #[serde(default)]
    pub default_model: Option<String>,

    /// Glob patterns matched against the canonical absolute paths of CLAUDE.md
    /// instruction files; matching files are excluded from the loaded set (the
    /// managed-policy file is exempt). Read once per session via
    /// [`claude_md_load_context`].
    ///
    /// Deliberately serialized even when empty: [`save`] merges only the keys
    /// this struct writes, so a skipped field would leave a stale non-empty
    /// list on disk and make "clear the excludes" impossible to persist.
    #[serde(default)]
    pub claude_md_excludes: Vec<String>,

    /// Context optimization: tool discovery, history rewrite, pruning, and
    /// compact output knobs. Shadow mode collects metrics without affecting
    /// model-facing content.
    #[serde(default)]
    pub context_optimization: ContextOptimizationSettings,

    /// MCP server switches (settings UI). The registry is built once at
    /// startup, so changes apply on the next launch.
    #[serde(default)]
    pub mcp: McpSettings,

    /// Hashline Edit tool switches. Read once per harness build, so changes
    /// apply to new threads.
    #[serde(default)]
    pub edit: EditSettings,

    /// Sandbox switches. The network allowlist is read per enable (not
    /// cached), so a session's network switch picks up edits immediately,
    /// and it is the one sandbox key written at runtime (approved-host
    /// amendments persist there).
    #[serde(default)]
    pub sandbox: SandboxSettings,
}

/// Sandbox switches: the network domain allowlist for the managed-proxy
/// route. Semantics: exact hosts, `*.example.com` (any subdomain),
/// `**.example.com` (apex plus subdomains), and IP literals; a hostname
/// that resolves into the host's own address space is refused even when
/// allowlisted. Deliberately serialized even when empty — a cleared
/// allowlist must reach disk.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SandboxSettings {
    #[serde(default)]
    pub network: SandboxNetworkSettings,
}

/// The `[sandbox.network]` table: `allow = ["crates.io", "*.example.com"]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SandboxNetworkSettings {
    pub allow: Vec<String>,
}

/// The configured network allowlist, read fresh (no cache): the managed
/// route compiles it per enable, so a hand edit between switches applies
/// without a restart.
pub fn sandbox_network_allow() -> Vec<String> {
    load().sandbox.network.allow
}

/// Append one host to the allowlist and persist it (the approved-host
/// amendment path). Read-modify-write serialized behind a process lock, the
/// same discipline as the MCP toggle: two concurrent amendments must not
/// drop each other's entry.
pub fn add_sandbox_network_allow(host: &str) -> Result<()> {
    static MODIFY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = MODIFY_LOCK.lock();
    let mut settings = load();
    if settings
        .sandbox
        .network
        .allow
        .iter()
        .any(|h| h.trim().eq_ignore_ascii_case(host.trim()))
    {
        return Ok(());
    }
    settings.sandbox.network.allow.push(host.to_string());
    save(&settings)
}

/// Hashline Edit tool switches. `enforce_seen_lines` opts a host into the
/// anti-blind-edit guard; it ships off because
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
/// The read-modify-write is not file-locked. Both this function and the host's
/// `ui_language` writer are driven by explicit settings-panel actions on the
/// main thread, so the window is small; a background writer would need a shared
/// lock (see the file-level concurrent-write convention for other stores).
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

/// Render `settings` as a document to write, preserving every key this crate
/// does not write.
///
/// The merge is intentionally additive at the top level: only the keys present
/// in the serialized [`Settings`] are overwritten. Two consequences follow.
/// Keys the host owns (`ui_language`) survive a runtime save, and keys this
/// crate does not write are never reclaimed — a stale `agent_language` or
/// `follow_up_behavior` simply stays in the file, unread and harmless, rather
/// than being silently deleted along with anything else we failed to model.
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

    /// Clearing `claude_md_excludes` must actually reach disk. The merge only
    /// overwrites keys this struct serializes, so a field skipped when empty
    /// would leave the previous non-empty list in place and silently discard
    /// the clear.
    #[test]
    fn clearing_excludes_is_persisted() {
        let existing = "claude_md_excludes = [\"vendor/**\"]\n";
        let cleared = Settings {
            claude_md_excludes: Vec::new(),
            ..Default::default()
        };

        let out = merge_into_existing(&cleared, Some(existing)).unwrap();

        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed
                .get("claude_md_excludes")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(0),
            "clearing the excludes did not reach disk:\n{out}"
        );
    }

    // ── context optimization tests ──────────────────────────────────

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
    fn sandbox_network_allowlist_round_trips() {
        let raw = r#"
ui_language = "en"

[sandbox.network]
allow = ["crates.io", "*.example.com"]
"#;
        let s: Settings = toml::from_str(raw).unwrap();
        assert_eq!(
            s.sandbox.network.allow,
            vec!["crates.io".to_string(), "*.example.com".to_string()]
        );
        // An absent table defaults to an empty (still-serialized) list.
        let empty: Settings = toml::from_str("ui_language = \"en\"").unwrap();
        assert!(empty.sandbox.network.allow.is_empty());
    }

    #[test]
    fn saving_an_empty_allowlist_clears_it_on_disk() {
        let existing = "[sandbox.network]\nallow = [\"old.example.com\"]\n";
        let cleared = Settings::default();
        let out = merge_into_existing(&cleared, Some(existing)).unwrap();
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let allow = parsed
            .get("sandbox")
            .and_then(|t| t.get("network"))
            .and_then(|t| t.get("allow"))
            .and_then(|v| v.as_array())
            .map(Vec::len);
        assert_eq!(
            allow,
            Some(0),
            "clearing the allowlist did not reach disk:\n{out}"
        );
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

"#;
        let s: Settings = toml::from_str(raw).unwrap();
        assert_eq!(s.context_optimization.tool_discovery, Toggle::Shadow);
    }
}
