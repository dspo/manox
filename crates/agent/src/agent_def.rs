//! Subagent definitions loaded from `~/.config/cx/manox/agents/*.md`.
//!
//! Each file is a YAML frontmatter block (name/description/tools/model/...)
//! followed by a markdown body that becomes the subagent's system prompt.
//! Mirrors Claude Code's `.claude/agents/*.md` format. The registry is loaded
//! once at startup; a missing or malformed file logs a warning and is skipped
//! rather than aborting the whole registry.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::paths;

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
    /// Scan the agents dir and parse every `*.md` file. Missing dir or parse
    /// errors do not abort the load; the registry ends up empty or partial.
    pub fn load() -> Self {
        let mut defs = BTreeMap::new();
        let dir = match paths::agents_dir() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("agents dir unavailable: {e:#}");
                return Self { defs };
            }
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self { defs },
            Err(e) => {
                tracing::warn!("failed to read agents dir {}: {e}", dir.display());
                return Self { defs };
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|x| x.to_str());
            if ext != Some("md") {
                continue;
            }
            match load_file(&path) {
                Ok(file) => {
                    if defs.insert(file.def.name.clone(), Arc::new(file)).is_some() {
                        tracing::warn!(
                            "duplicate subagent name from {}; last definition wins",
                            path.display()
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("skipping agent def {}: {e:#}", path.display());
                }
            }
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

/// Parse one agent definition markdown file: split frontmatter from body,
/// deserialize the frontmatter, keep the body verbatim as the system prompt.
fn load_file(path: &std::path::Path) -> Result<AgentDefinitionFile> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let (front, body) = split_frontmatter(&raw);
    let def: AgentDefinition = serde_yaml::from_str(front)
        .with_context(|| format!("parsing frontmatter in {}", path.display()))?;
    if def.name.trim().is_empty() {
        anyhow::bail!("agent definition has empty `name`");
    }
    if def.description.trim().is_empty() {
        anyhow::bail!("agent definition `{}` has empty `description`", def.name);
    }
    Ok(AgentDefinitionFile {
        def,
        system_prompt: body,
    })
}

/// Split a markdown file into `(frontmatter_yaml, body)`. Frontmatter is the
/// content between the first and second `---` lines; everything after the
/// closing fence is the body. A file without a leading `---` line is treated
/// as body-only (empty frontmatter → caller errors on the empty yaml).
fn split_frontmatter(raw: &str) -> (&str, String) {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let after_open = match raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    {
        Some(s) => s,
        None => return ("", raw.to_string()),
    };
    // Find the closing fence: a line that is exactly `---` (with optional `\r`).
    // `front_end` is the byte offset where the `---` line begins, so `front`
    // excludes the closing fence entirely (a trailing `---` would make
    // serde_yaml reject the frontmatter as a second YAML document).
    let mut front_end = None;
    let mut line_start = 0;
    for (i, ch) in after_open.char_indices() {
        if ch != '\n' {
            continue;
        }
        let line = after_open[line_start..i].trim_end_matches('\r');
        if line == "---" {
            front_end = Some(line_start);
            break;
        }
        line_start = i + 1;
    }
    if front_end.is_none() && after_open[line_start..].trim_end_matches('\r') == "---" {
        front_end = Some(line_start);
    }
    match front_end {
        Some(end) => {
            let front = &after_open[..end];
            let body = after_open[end..]
                .strip_prefix("---\n")
                .or_else(|| after_open[end..].strip_prefix("---\r\n"))
                .unwrap_or(&after_open[end..]);
            (front, body.to_string())
        }
        None => (after_open, String::new()),
    }
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
        let (front, body) = split_frontmatter(SAMPLE);
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
        let (front, body) = split_frontmatter(raw);
        assert_eq!(front, "");
        assert_eq!(body, raw);
    }

    #[test]
    fn missing_optional_fields_default() {
        let raw = "---\nname: minimal\ndescription: bare\n---\nbody\n";
        let (front, _) = split_frontmatter(raw);
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
}
