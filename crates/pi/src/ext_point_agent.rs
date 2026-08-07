// Agent extension point — static agent definitions as manifests.
//
// An `AgentDef` is pure data: defining an agent is writing a manifest (YAML
// frontmatter + markdown body), not code. A def becomes a subagent only at
// runtime, when the main agent invokes it through the `agent` tool.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// Static agent definition parsed from a manifest.
///
/// `name`/`description`/`tools`/`model` come from the YAML frontmatter; the
/// markdown body becomes the agent's system prompt. Unknown frontmatter
/// fields are rejected rather than silently ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDef {
    /// Registry key, also the `subagent_type` the main agent passes.
    pub name: String,
    /// Model-facing description of what the agent is for.
    pub description: String,
    /// Tool names this agent may use, resolved against the caller's tool
    /// snapshot at dispatch time. Empty means "all tools".
    #[serde(default)]
    pub tools: Vec<String>,
    /// Optional model override. Resolution is the caller's job (the core is
    /// registry-free); the field is declared here so manifests stay
    /// self-describing.
    #[serde(default)]
    pub model: Option<String>,
    /// The agent's system prompt (manifest body).
    #[serde(skip)]
    pub system_prompt: String,
}

impl AgentDef {
    /// Parse a manifest: `---`-delimited YAML frontmatter followed by the
    /// markdown body that becomes the system prompt.
    pub fn parse_md(input: &str) -> Result<Self, String> {
        let (frontmatter, body) = split_frontmatter(input)?;
        let mut def: AgentDef =
            serde_yaml::from_str(&frontmatter).map_err(|e| format!("invalid frontmatter: {e}"))?;
        def.system_prompt = body.trim().to_string();
        Ok(def)
    }
}

/// Split `---\n...\n---\nbody` into the frontmatter YAML and the body.
///
/// The closing delimiter must be a whole `---` line; a BOM is stripped; an
/// empty body is rejected so a manifest never yields an empty system prompt.
fn split_frontmatter(input: &str) -> Result<(String, String), String> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let rest = input
        .strip_prefix("---")
        .ok_or_else(|| "manifest must start with `---`".to_string())?;
    let rest = rest
        .strip_prefix('\n')
        .ok_or_else(|| "manifest frontmatter must start on the next line".to_string())?;
    let mut frontmatter = String::new();
    let mut body_start: Option<usize> = None;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            body_start = Some(offset + line.len());
            break;
        }
        frontmatter.push_str(line);
        offset += line.len();
    }
    let body = match body_start {
        Some(start) => &rest[start..],
        None => return Err("unterminated frontmatter delimiter".to_string()),
    };
    if body.trim().is_empty() {
        return Err("manifest body is empty".to_string());
    }
    Ok((frontmatter, body.to_string()))
}

/// Registry of agent definitions, keyed by name.
#[derive(Default)]
pub struct AgentRegistry {
    agents: HashMap<String, AgentDef>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a definition; a later definition with the same name replaces
    /// the earlier one.
    pub fn register(&mut self, def: AgentDef) {
        self.agents.insert(def.name.clone(), def);
    }

    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.agents.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.agents.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        names
    }

    pub fn all(&self) -> Vec<&AgentDef> {
        self.agents.values().collect()
    }

    /// Load every `*.md` manifest in a directory, in deterministic filename
    /// order so a later file with the same name wins. A missing directory is
    /// a no-op; a malformed file is skipped with a log rather than aborting
    /// the whole load.
    pub fn from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut files: Vec<_> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
            .collect();
        files.sort_by_key(|e| e.file_name());
        for entry in files {
            let path = entry.path();
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            match AgentDef::parse_md(&content) {
                Ok(def) => {
                    tracing::debug!(agent = %def.name, "loaded agent definition");
                    self.register(def);
                }
                Err(e) => tracing::warn!(path = %path.display(), "skipping agent manifest: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_md_extracts_frontmatter_and_body() {
        let manifest = "---\nname: Explore\ndescription: Read-only search\n\
                        tools:\n  - Read\n  - Grep\n  - Find\n  - Ls\n---\n\
                        You are the Explore agent.\n\nSearch carefully.\n";
        let def = AgentDef::parse_md(manifest).unwrap();
        assert_eq!(def.name, "Explore");
        assert_eq!(def.description, "Read-only search");
        assert_eq!(def.tools, vec!["Read", "Grep", "Find", "Ls"]);
        assert!(def.model.is_none());
        assert_eq!(
            def.system_prompt,
            "You are the Explore agent.\n\nSearch carefully."
        );
    }

    #[test]
    fn parse_md_accepts_optional_model_and_empty_tools() {
        let manifest = "---\nname: Worker\ndescription: d\nmodel: qwen3.7-max\n---\nbody";
        let def = AgentDef::parse_md(manifest).unwrap();
        assert_eq!(def.model.as_deref(), Some("qwen3.7-max"));
        assert!(def.tools.is_empty());
    }

    #[test]
    fn parse_md_rejects_unknown_frontmatter_fields() {
        let manifest = "---\nname: X\ndescription: d\ntypo_field: 1\n---\nbody";
        assert!(AgentDef::parse_md(manifest).is_err());
    }

    #[test]
    fn parse_md_requires_delimiters() {
        assert!(AgentDef::parse_md("no frontmatter").is_err());
        let unterminated = "---\nname: X\ndescription: d\nbody without close";
        assert!(AgentDef::parse_md(unterminated).is_err());
        // An empty body must not yield an empty system prompt.
        let empty_body = "---\nname: X\ndescription: d\n---\n";
        assert!(AgentDef::parse_md(empty_body).is_err());
    }

    #[test]
    fn parse_md_strips_bom_and_rejects_non_whole_line_close() {
        let bom = "\u{feff}---\nname: X\ndescription: d\n---\nbody";
        assert_eq!(AgentDef::parse_md(bom).unwrap().name, "X");
        // A `---` inside a frontmatter scalar must not close the block.
        let inner = "---\nname: X\ndescription: |-\n  --- not close\n---\nbody";
        assert_eq!(AgentDef::parse_md(inner).unwrap().name, "X");
    }

    #[test]
    fn registry_registers_and_overrides() {
        let mut registry = AgentRegistry::new();
        let a = AgentDef {
            name: "A".into(),
            description: "one".into(),
            tools: vec![],
            model: None,
            system_prompt: "p".into(),
        };
        registry.register(a.clone());
        registry.register(AgentDef {
            name: "A".into(),
            description: "two".into(),
            tools: vec!["Read".into()],
            model: None,
            system_prompt: "p2".into(),
        });
        assert_eq!(registry.names(), vec!["A"]);
        assert_eq!(registry.get("A").unwrap().description, "two");
        assert_eq!(registry.get("missing"), None);
    }

    #[test]
    fn from_dir_loads_manifests_and_skips_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("good.md"),
            "---\nname: Good\ndescription: d\n---\nbody",
        )
        .unwrap();
        fs::write(dir.path().join("bad.md"), "not a manifest").unwrap();
        fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let mut registry = AgentRegistry::new();
        registry.from_dir(dir.path());
        assert_eq!(registry.names(), vec!["Good"]);
    }
}
