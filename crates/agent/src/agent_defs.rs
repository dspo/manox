//! User-authored and plugin-provided subagent definitions (ported from the
//! retired manox harness's `agent_def` loader).
//!
//! The built-in `Explore` definition ships in `pi-extensions`; this loader
//! adds two discovery layers on top at session-build time:
//!
//! - **User-authored** files under `~/.manox/agents/*.md`
//!   (Claude Code `.claude/agents/*.md` format: YAML frontmatter +
//!   markdown system prompt). A user file with the same `name` overrides
//!   the built-in (registry insert replaces), so users can customize the
//!   bundled agents.
//! - **Plugin-provided** files under each installed plugin's `agents/`
//!   directory, registered as `<plugin>:<name>` so they never collide with
//!   built-in or user agents — the parent model passes
//!   `subagent_type: "gitwork:reviewer"` to delegate to a plugin agent,
//!   matching Claude Code's plugin-scoped lookup.
//!
//! A missing directory is normal (no definitions); a malformed file logs a
//! warning and is skipped rather than aborting the whole registry.

use std::path::{Path, PathBuf};

use pi::ext_point_agent::{AgentDef, AgentRegistry};

/// `~/.manox/agents` — user-authored subagent definitions.
pub fn user_agents_dir() -> Result<PathBuf, anyhow::Error> {
    Ok(crate::paths::manox_config_dir()?.join("agents"))
}

/// Parse every `*.md` definition in `dir`; malformed files warn and skip.
/// Returns `(file label, definition)` pairs in directory order.
fn load_dir(dir: &Path) -> Vec<(String, AgentDef)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let label = path.display().to_string();
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!("agent definition {label} unreadable: {err}");
                continue;
            }
        };
        match AgentDef::parse_md(&content) {
            Ok(def) => out.push((label, def)),
            Err(err) => {
                tracing::warn!("agent definition {label} malformed, skipped: {err}");
            }
        }
    }
    out
}

/// Register user-authored definitions (same-name overrides built-ins), then
/// plugin definitions namespaced `<plugin>:<name>`.
pub fn register_user_and_plugin(registry: &mut AgentRegistry) {
    register_from_dirs(
        registry,
        user_agents_dir().ok().as_deref(),
        &crate::plugin::PluginManager::installed(),
    );
}

/// Testable core: `user_dir` for user-authored definitions, `plugins`
/// whose `agents/` subdirectories are namespaced.
fn register_from_dirs(
    registry: &mut AgentRegistry,
    user_dir: Option<&Path>,
    plugins: &[crate::plugin::InstalledPlugin],
) {
    if let Some(dir) = user_dir {
        for (label, def) in load_dir(dir) {
            tracing::debug!(agent = %def.name, source = %label, "registered user agent definition");
            registry.register(def);
        }
    }
    for plugin in plugins {
        for (label, mut def) in load_dir(&plugin.root.join("agents")) {
            def.name = format!("{}:{}", plugin.name, def.name);
            tracing::debug!(agent = %def.name, source = %label, "registered plugin agent definition");
            registry.register(def);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_DEF: &str = "---\nname: Reviewer\ndescription: Reviews diffs.\ntools:\n  - Read\n  - Grep\n---\nYou review diffs.\n";

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn user_dir_definitions_register_by_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "reviewer.md", VALID_DEF);
        let mut registry = AgentRegistry::new();
        register_from_dirs(&mut registry, Some(dir.path()), &[]);
        assert_eq!(registry.names(), vec!["Reviewer"]);
        assert_eq!(
            registry.get("Reviewer").unwrap().description,
            "Reviews diffs."
        );
    }

    #[test]
    fn malformed_and_non_md_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "good.md", VALID_DEF);
        write(dir.path(), "broken.md", "no frontmatter here");
        write(dir.path(), "notes.txt", VALID_DEF);
        let mut registry = AgentRegistry::new();
        register_from_dirs(&mut registry, Some(dir.path()), &[]);
        assert_eq!(registry.names(), vec!["Reviewer"]);
    }

    #[test]
    fn same_name_user_def_overrides_earlier_registration() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "explore-override.md",
            "---\nname: Explore\ndescription: Custom explore.\ntools:\n  - Read\n---\nCustom body.\n",
        );
        let mut registry = AgentRegistry::new();
        // Simulate the built-in registered first.
        registry.register(
            AgentDef::parse_md("---\nname: Explore\ndescription: Built-in.\ntools:\n  - Read\n---\nBuilt-in body.\n")
                .unwrap(),
        );
        register_from_dirs(&mut registry, Some(dir.path()), &[]);
        assert_eq!(registry.names(), vec!["Explore"]);
        assert_eq!(
            registry.get("Explore").unwrap().description,
            "Custom explore."
        );
    }

    #[test]
    fn plugin_definitions_are_namespaced() {
        let plugin_root = tempfile::tempdir().unwrap();
        let agents = plugin_root.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        write(&agents, "reviewer.md", VALID_DEF);
        let mut registry = AgentRegistry::new();
        register_from_dirs(
            &mut registry,
            None,
            &[crate::plugin::InstalledPlugin {
                name: "gitwork".to_string(),
                root: plugin_root.path().to_path_buf(),
                marketplace: "test".to_string(),
            }],
        );
        assert_eq!(registry.names(), vec!["gitwork:Reviewer"]);
    }

    #[test]
    fn missing_dirs_register_nothing() {
        let mut registry = AgentRegistry::new();
        register_from_dirs(
            &mut registry,
            Some(Path::new("/nonexistent/agents-dir")),
            &[],
        );
        assert!(registry.names().is_empty());
    }
}
