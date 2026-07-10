//! A plugin's `hooks/hooks.json` maps lifecycle events (`SessionStart`,
//! `SessionEnd`, `Stop`, `PreToolUse`, `PostToolUse`) to shell commands. The
//! command runs with `CLAUDE_PLUGIN_ROOT` set to the plugin's installed root,
//! `CLAUDE_PROJECT_DIR` set to the owning thread's cwd, and the event payload
//! fed on stdin as JSON — the same contract Claude Code exposes, so a plugin's
//! `scripts/*.mjs` handlers run unchanged under manox.
//!
//! `SessionStart` fires before a main thread's first turn; `SessionEnd` fires
//! when a thread is deleted (its session is over). `Stop` fires on each turn's
//! end. Hooks are fire-and-forget and fail-open: a handler error or timeout is
//! logged and never blocks the turn. This matches the fail-open discipline
//! Claude Code applies to its Stop-gate hooks (a broken reviewer must not hang
//! the session). `PreToolUse` / `PostToolUse` therefore cannot block a tool
//! call in this implementation — they are notification-only. The full
//! decision-returning hook protocol is a future extension; the current surface
//! covers every hook the shipped marketplace plugins actually use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::plugin::PluginManager;

/// Default cap on a hook's wall-clock runtime when the entry declares no
/// `timeout`. Without a default, a buggy handler that loops forever would run
/// forever (its tokio task is detached); the cap turns that into a logged
/// timeout. Generous so legitimate slow handlers (e.g. `npm install` in a
/// SessionStart) are not false-killed.
const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 300;

/// Lifecycle events a plugin hook can subscribe to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    Stop,
    PreToolUse,
    PostToolUse,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Stop => "Stop",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
        }
    }
}

/// One shell command to run for an event, with an optional timeout (seconds).
#[derive(Debug, Clone, Deserialize)]
pub struct HookEntry {
    pub command: String,
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// Intermediate shape mirroring the JSON file: each event maps to a list of
/// groups, each group carrying a `hooks` array. Flattened on load.
#[derive(Debug, Clone, Deserialize)]
struct HookGroup {
    hooks: Vec<HookEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct HooksFile {
    #[serde(default)]
    hooks: BTreeMap<String, Vec<HookGroup>>,
}

/// A plugin's loaded hook config: event name → flat list of entries.
#[derive(Debug, Clone, Default)]
pub struct HookConfig {
    entries: BTreeMap<String, Vec<HookEntry>>,
}

impl HookConfig {
    /// Load `hooks/hooks.json` from a plugin root. Returns `None` when the file
    /// is absent (hooks are optional) or malformed (warn-logged, treated as
    /// absent so one bad plugin cannot poison the registry).
    pub fn load(plugin_root: &Path) -> Option<Self> {
        let path = plugin_root.join("hooks").join("hooks.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("hooks.json read failed for {}: {e}", plugin_root.display());
                return None;
            }
        };
        let file: HooksFile = match serde_json::from_str(&raw) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("hooks.json parse failed for {}: {e}", plugin_root.display());
                return None;
            }
        };
        let mut entries = BTreeMap::new();
        for (event, groups) in file.hooks {
            let flat: Vec<HookEntry> = groups.into_iter().flat_map(|g| g.hooks).collect();
            entries.insert(event, flat);
        }
        Some(HookConfig { entries })
    }

    pub fn for_event(&self, event: HookEvent) -> &[HookEntry] {
        self.entries
            .get(event.as_str())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

/// One installed plugin's hook config plus the env it needs.
#[derive(Debug, Clone)]
struct InstalledHooks {
    plugin_name: String,
    root: PathBuf,
    config: HookConfig,
}

/// Process-wide registry of all plugin hooks, loaded once at startup.
#[derive(Debug, Default)]
pub struct HookRegistry {
    plugins: Vec<InstalledHooks>,
}

impl HookRegistry {
    pub fn load() -> Self {
        let mut plugins = Vec::new();
        for plugin in PluginManager::installed() {
            if let Some(config) = HookConfig::load(&plugin.root) {
                plugins.push(InstalledHooks {
                    plugin_name: plugin.name.clone(),
                    root: plugin.root.clone(),
                    config,
                });
            }
        }
        Self { plugins }
    }

    /// Fire `event` for every plugin that subscribes to it. Each command runs
    /// detached on the global tokio runtime; failures and timeouts are logged
    /// and never propagated — the turn proceeds regardless (fail-open).
    /// `project_cwd` becomes `CLAUDE_PROJECT_DIR` for the handler, matching the
    /// Claude Code contract (the user's project, not the plugin install dir).
    pub fn fire(&self, event: HookEvent, project_cwd: Option<&str>, payload: Value) {
        let handle = crate::runtime::handle().clone();
        let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default();
        for plugin in &self.plugins {
            for entry in plugin.config.for_event(event) {
                let plugin_name = plugin.plugin_name.clone();
                let root = plugin.root.clone();
                let command = entry.command.clone();
                let timeout = entry.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS);
                let payload = payload_bytes.clone();
                let project_cwd = project_cwd.map(|s| s.to_string());
                handle.spawn(async move {
                    run_hook(
                        &plugin_name,
                        &root,
                        &command,
                        &payload,
                        project_cwd.as_deref(),
                        timeout,
                    )
                    .await;
                });
            }
        }
    }
}

async fn run_hook(
    plugin_name: &str,
    plugin_root: &Path,
    command: &str,
    payload: &[u8],
    project_cwd: Option<&str>,
    timeout_secs: u64,
) {
    // Hooks are fire-and-forget outside the tool approval loop (fail-open by
    // design). Cross-app automation commands would otherwise silently broker
    // Apple Events from a plugin; flag them so the attempt is at least
    // auditable in the trace log, even though it is not blocked here.
    if crate::sandbox::is_cross_app_automation(command) {
        tracing::warn!(
            plugin = plugin_name,
            command = command,
            "hook runs cross-app automation command (osascript / AppleScript / `open -a`) \
             — not approval-gated; auditing only",
        );
    }
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(command);
    cmd.env("CLAUDE_PLUGIN_ROOT", plugin_root);
    // `CLAUDE_PROJECT_DIR` is the user's project, not the plugin install dir.
    // Fall back to the plugin root's parent only when the caller has no thread
    // cwd (early boot paths) — better a rough guess than an unset var.
    let fallback = plugin_root.parent().map(|p| p.to_path_buf());
    let project_dir = project_cwd
        .map(PathBuf::from)
        .or(fallback)
        .unwrap_or_else(|| plugin_root.to_path_buf());
    cmd.env("CLAUDE_PROJECT_DIR", project_dir);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Kill the sh + its children when the task is dropped (e.g. on timeout) so
    // a hung handler cannot orphan a process beyond the deadline.
    cmd.kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(plugin = plugin_name, "hook spawn failed ({command}): {e}");
            return;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload).await;
        // Drop stdin to signal EOF; the handler reads the payload then exits.
        drop(stdin);
    }

    let fut = child.wait_with_output();
    let result = match tokio::time::timeout(Duration::from_secs(timeout_secs), fut).await {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!(
                plugin = plugin_name,
                secs = timeout_secs,
                "hook timeout ({command})"
            );
            return;
        }
    };

    match result {
        Ok(out) if !out.status.success() => {
            tracing::warn!(
                plugin = plugin_name,
                code = ?out.status.code(),
                "hook non-zero exit ({command}): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(plugin = plugin_name, "hook wait failed ({command}): {e}");
        }
    }
}

static REGISTRY: OnceLock<HookRegistry> = OnceLock::new();

pub fn init() {
    let registry = HookRegistry::load();
    if let Err(existing) = REGISTRY.set(registry) {
        tracing::warn!("hook registry already initialized");
        let _ = existing;
    }
}

fn registry() -> Option<&'static HookRegistry> {
    REGISTRY.get()
}

/// Fire an event across all plugins. `project_cwd` is exposed to handlers as
/// `CLAUDE_PROJECT_DIR`; pass the owning thread's cwd so a handler sees the
/// user's project, not the plugin install dir. No-op when no hooks registered.
pub fn fire(event: HookEvent, project_cwd: Option<&str>, payload: Value) {
    if let Some(reg) = registry() {
        reg.fire(event, project_cwd, payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_flat_hooks() {
        let raw = r#"{
            "hooks": {
                "Stop": [{"hooks":[{"type":"command","command":"node x.mjs","timeout":200}]}],
                "SessionStart": [{"hooks":[{"command":"echo hi"}]}]
            }
        }"#;
        let f: HooksFile = serde_json::from_str(raw).unwrap();
        assert_eq!(f.hooks.len(), 2);
        assert_eq!(f.hooks["Stop"][0].hooks.len(), 1);
        assert_eq!(f.hooks["Stop"][0].hooks[0].command, "node x.mjs");
        assert_eq!(f.hooks["Stop"][0].hooks[0].timeout, Some(200));
    }

    #[test]
    fn for_event_returns_empty_when_unsubscribed() {
        let cfg = HookConfig::default();
        assert!(cfg.for_event(HookEvent::Stop).is_empty());
    }

    #[test]
    fn event_as_str_matches_wire_names() {
        assert_eq!(HookEvent::SessionStart.as_str(), "SessionStart");
        assert_eq!(HookEvent::PreToolUse.as_str(), "PreToolUse");
        assert_eq!(HookEvent::PostToolUse.as_str(), "PostToolUse");
    }
}
