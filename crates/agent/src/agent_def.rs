//! Subagent definitions: a set of built-in definitions compiled into the
//! binary (`plan`, `explore`) plus user-authored files under
//! `~/.config/cx/manox/agents/*.md` and the `agents/` subdirectory of every
//! installed plugin.
//!
//! Each file is a YAML frontmatter block (name/description/tools/model/...)
//! followed by a markdown body that becomes the subagent's system prompt.
//! Mirrors Claude Code's `.claude/agents/*.md` format. The registry is loaded
//! once at startup; a missing or malformed file logs a warning and is skipped
//! rather than aborting the whole registry.
//!
//! Built-in definitions are loaded first; a user-authored or plugin file with
//! the same `name` overrides the built-in (same-key-wins on insert order), so
//! users can customize or replace the bundled `plan`/`explore` agents.
//!
//! Plugin-provided definitions are registered under a `plugin:name` namespace
//! so they never collide with built-in or user-authored agents — the parent
//! model passes `subagent_type: "gitwork:reviewer"` to delegate to a plugin
//! agent, matching Claude Code's plugin-scoped lookup.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::paths;
use crate::plugin::PluginManager;

/// A single subagent definition (frontmatter only).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentDefinition {
    /// Unique key; the `subagent_type` the parent model passes to the `agent` tool.
    pub name: String,
    /// One-line description the parent model reads to decide when to delegate.
    pub description: String,
    /// Tool whitelist. `None` = inherit all built-in tools. Does not affect the
    /// `agent` tool — that is governed solely by `allow_nesting` (and the depth
    /// cap), so listing `agent` here is a no-op and listing it in
    /// `disallowed_tools` will not block nesting.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Tool blacklist. Takes precedence over `tools` when both are present.
    /// Ignored for the `agent` tool (see `tools`).
    #[serde(default)]
    pub disallowed_tools: Option<Vec<String>>,
    /// Model id resolvable via `ProviderRegistry::get_model`. `None` = inherit
    /// the parent `Thread`'s model.
    #[serde(default)]
    pub model: Option<String>,
    /// Max agentic turns before the subagent is force-stopped. Defaults to 10
    /// when `None` at spawn time.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Whether the subagent may itself spawn sub-agents (nesting). Defaults false.
    #[serde(default)]
    pub allow_nesting: bool,
}

/// A loaded definition file: frontmatter + the body used as system prompt.
#[derive(Debug, Clone)]
pub struct AgentDefinitionFile {
    pub def: AgentDefinition,
    pub system_prompt: String,
}

/// Process-wide registry of subagent definitions, keyed by `name`.
#[derive(Debug, Default)]
pub struct AgentDefinitionRegistry {
    defs: BTreeMap<String, Arc<AgentDefinitionFile>>,
}

impl AgentDefinitionRegistry {
    /// Load the registry: built-in definitions first, then user-authored and
    /// plugin files. Same-`name` later loads override earlier ones, so users can
    /// customize the bundled `plan`/`explore` agents by dropping a same-named
    /// file in `~/.config/cx/manox/agents/`. Missing dirs or parse errors do not
    /// abort the load; the registry ends up partial. Plugin definitions are
    /// registered under `plugin:name` so they cannot shadow user-authored ones.
    pub fn load() -> Self {
        let mut defs = BTreeMap::new();
        // Built-in definitions compiled into the binary. Inserted first so user
        // and plugin definitions with the same `name` override them.
        for file in builtin_definitions() {
            defs.insert(file.def.name.clone(), Arc::new(file));
        }
        // User-authored definitions: bare frontmatter `name`, no namespace.
        if let Ok(dir) = paths::agents_dir() {
            scan_dir(&dir, &mut |path| match load_file(path) {
                Ok(file) => {
                    defs.insert(file.def.name.clone(), Arc::new(file));
                }
                Err(e) => tracing::warn!("skipping agent def {}: {e:#}", path.display()),
            });
        }
        // Plugin definitions: `plugin:name` namespace.
        for plugin in PluginManager::installed() {
            let dir = plugin.root.join("agents");
            if !dir.exists() {
                continue;
            }
            let ns = plugin.name.clone();
            scan_dir(&dir, &mut |path| match load_file(path) {
                Ok(file) => {
                    let key = format!("{ns}:{}", file.def.name);
                    defs.insert(key, Arc::new(file));
                }
                Err(e) => tracing::warn!("skipping agent def {}: {e:#}", path.display()),
            });
        }
        Self { defs }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<AgentDefinitionFile>> {
        self.defs.get(name)
    }

    pub fn list(&self) -> Vec<&Arc<AgentDefinitionFile>> {
        self.defs.values().collect()
    }
}

/// Walk a directory once, calling `on_file` for each top-level `*.md` entry.
/// Missing dir is silent (the user/plugin may simply have no definitions).
fn scan_dir(dir: &std::path::Path, on_file: &mut dyn FnMut(&std::path::Path)) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!("failed to read agents dir {}: {e}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|x| x.to_str()) == Some("md") {
            on_file(&path);
        }
    }
}

/// Parse one agent definition markdown file: split frontmatter from body,
/// deserialize the frontmatter, keep the body verbatim as the system prompt.
fn load_file(path: &std::path::Path) -> Result<AgentDefinitionFile> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_definition(&raw, &format!("{}", path.display()))
}

/// Parse a definition from a raw markdown string. `source` is used only for
/// error context (a path or a builtin id). Shared by the file loader and the
/// built-in definition loader so they apply the same validation.
fn parse_definition(raw: &str, source: &str) -> Result<AgentDefinitionFile> {
    let parsed = crate::frontmatter::parse::<AgentDefinition>(raw)
        .map_err(|e| anyhow::anyhow!("parsing frontmatter in {source}: {e:#}"))?;
    let def = parsed.front;
    if def.name.trim().is_empty() {
        anyhow::bail!("agent definition has empty `name`");
    }
    if def.description.trim().is_empty() {
        anyhow::bail!("agent definition `{}` has empty `description`", def.name);
    }
    Ok(AgentDefinitionFile {
        def,
        system_prompt: parsed.body,
    })
}

/// The built-in subagent definitions compiled into the binary. Each entry is
/// `include_str!`-embedded markdown (frontmatter + body) living next to the
/// source, mirroring `system_prompt.md`. A malformed builtin is a compile-time
/// authoring error, so failures are surfaced as panics at load time rather than
/// silently skipped.
fn builtin_definitions() -> Vec<AgentDefinitionFile> {
    const PLAN: &str = include_str!("agents/plan.md");
    const EXPLORE: &str = include_str!("agents/explore.md");
    vec![
        parse_definition(PLAN, "builtin:plan").expect("builtin plan agent must parse"),
        parse_definition(EXPLORE, "builtin:explore").expect("builtin explore agent must parse"),
    ]
}

static REGISTRY: OnceLock<AgentDefinitionRegistry> = OnceLock::new();

/// Load the registry into the process-global slot. Called from `agent::init`.
/// Failures are logged and an empty registry is installed so the app still runs.
pub fn init() {
    let registry = AgentDefinitionRegistry::load();
    if let Err(existing) = REGISTRY.set(registry) {
        tracing::warn!(
            "agent definition registry already initialized ({} defs)",
            existing.list().len()
        );
    }
}

/// Access the process-global registry. Panics only if `init` was never called.
pub fn global() -> &'static AgentDefinitionRegistry {
    REGISTRY
        .get()
        .expect("agent definition registry not initialized")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nname: researcher\ndescription: 只读代码探索\ntools:\n  - read_file\n  - grep\ndisallowed_tools:\n  - bash\nmax_turns: 20\nallow_nesting: false\n---\nYou are a code research specialist.\nBe thorough.\n";

    #[test]
    fn parses_frontmatter_and_body() {
        let (front, body) = crate::frontmatter::split(SAMPLE);
        let def: AgentDefinition = serde_yaml::from_str(front).unwrap();
        assert_eq!(def.name, "researcher");
        assert_eq!(def.description, "只读代码探索");
        assert_eq!(
            def.tools.as_deref(),
            Some(&["read_file".to_string(), "grep".to_string()][..])
        );
        assert_eq!(
            def.disallowed_tools.as_deref(),
            Some(&["bash".to_string()][..])
        );
        assert_eq!(def.max_turns, Some(20));
        assert!(!def.allow_nesting);
        assert_eq!(body, "You are a code research specialist.\nBe thorough.\n");
    }

    #[test]
    fn body_only_file_yields_empty_frontmatter() {
        let raw = "no frontmatter here\n";
        let (front, body) = crate::frontmatter::split(raw);
        assert_eq!(front, "");
        assert_eq!(body, raw);
    }

    #[test]
    fn missing_optional_fields_default() {
        let raw = "---\nname: minimal\ndescription: bare\n---\nbody\n";
        let (front, _) = crate::frontmatter::split(raw);
        let def: AgentDefinition = serde_yaml::from_str(front).unwrap();
        assert!(def.tools.is_none());
        assert!(def.disallowed_tools.is_none());
        assert!(def.model.is_none());
        assert!(def.max_turns.is_none());
        assert!(!def.allow_nesting);
    }

    #[test]
    fn load_file_rejects_empty_name() {
        let tmp = std::env::temp_dir().join("manox_agent_def_test.md");
        std::fs::write(&tmp, "---\nname: ''\ndescription: x\n---\nbody\n").unwrap();
        let r = load_file(&tmp);
        assert!(r.is_err());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn registry_handles_missing_dir() {
        // Point at a nonexistent subdir of temp; load must not panic.
        let r = AgentDefinitionRegistry::load();
        // No guarantee about contents, but it must return a registry (possibly empty).
        let _ = r.list();
    }

    #[test]
    fn builtin_definitions_parse_and_are_read_only() {
        let builtins = builtin_definitions();
        let names: Vec<&str> = builtins.iter().map(|f| f.def.name.as_str()).collect();
        assert!(
            names.contains(&"plan"),
            "builtin plan must exist: {names:?}"
        );
        assert!(
            names.contains(&"explore"),
            "builtin explore must exist: {names:?}"
        );
        for f in &builtins {
            // Read-only agents must disallow the write/spawn tools.
            let dis = f
                .def
                .disallowed_tools
                .as_ref()
                .expect("read-only builtin has disallowed_tools");
            for blocked in ["write_file", "edit_file", "bash", "agent"] {
                assert!(
                    dis.iter().any(|x| x == blocked),
                    "{} must disallow {blocked}",
                    f.def.name
                );
            }
            assert!(
                !f.system_prompt.trim().is_empty(),
                "{} has empty system prompt",
                f.def.name
            );
        }
    }
}
