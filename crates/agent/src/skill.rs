//! Skills — model-invokable reference documents.
//!
//! A skill is a markdown file (`SKILL.md`) with YAML frontmatter
//! (`name` / `description`) and a body the model reads on demand. Unlike a
//! slash command (user-triggered macro) or an agent (spawned sub-thread), a
//! skill is passive reference material: the model sees a one-line summary in
//! its system prompt and pulls the full body via the `skill` tool only when a
//! task calls for that knowledge. This mirrors Claude Code's Skill mechanism.
//!
//! Discovery mirrors the marketplace layout: each installed plugin's
//! `skills/<skill-name>/SKILL.md` is registered under `plugin:skill-name`, and
//! user-authored `~/.config/cx/manox/skills/<name>/SKILL.md` files use the bare
//! name. A plugin may also carry a root-level `SKILL.md` (the plugin's own
//! overview) — registered under the bare plugin name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::paths;
use crate::plugin::PluginManager;

#[derive(Debug, Clone, Deserialize)]
struct SkillMeta {
    name: String,
    #[serde(default)]
    description: String,
}

/// A loaded skill: frontmatter identity + the body the `skill` tool returns.
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub body: String,
    /// On-disk source, for diagnostics and re-reads.
    pub source: PathBuf,
}

/// Process-wide registry of skills, keyed by lookup name (`<plugin>:<skill>`
/// or bare `<skill>`). Loaded once at startup; malformed files are skipped.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, Arc<SkillDefinition>>,
}

impl SkillRegistry {
    pub fn load() -> Self {
        let mut skills = BTreeMap::new();
        // User-authored skills: bare name.
        if let Ok(dir) = paths::skills_dir() {
            scan_skills_root(&dir, None, &mut skills);
        }
        // Plugin skills: `plugin:name` namespace + a bare-name root SKILL.md.
        for plugin in PluginManager::installed() {
            let root = plugin.root.join("skills");
            if root.exists() {
                scan_skills_root(&root, Some(&plugin.name), &mut skills);
            }
            // A plugin-level root SKILL.md is the plugin's overview skill.
            let overview = plugin.root.join("SKILL.md");
            if overview.is_file()
                && let Ok(s) = load_skill_file(&overview)
            {
                skills.insert(plugin.name.clone(), Arc::new(s));
            }
        }
        Self { skills }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<SkillDefinition>> {
        self.skills.get(name)
    }

    pub fn list(&self) -> Vec<&Arc<SkillDefinition>> {
        self.skills.values().collect()
    }

    /// One-line summaries (`- name: description`) for the system prompt, so the
    /// model knows which skills exist without their full bodies in context.
    pub fn summary_block(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "## Available skills (consult their full body via the `skill` tool on demand)\n",
        );
        for s in self.skills.values() {
            out.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
        out
    }
}

/// Scan a `skills/` root for `<name>/SKILL.md` entries. `namespace` is the
/// plugin name when scanning a plugin, or `None` for user-authored skills.
fn scan_skills_root(
    root: &Path,
    namespace: Option<&str>,
    out: &mut BTreeMap<String, Arc<SkillDefinition>>,
) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!("failed to read skills dir {}: {e}", root.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        match load_skill_file(&skill_file) {
            Ok(mut s) => {
                // Directory name is the fallback when frontmatter omits `name`.
                if s.name.is_empty() {
                    s.name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string();
                }
                if s.name.is_empty() {
                    continue;
                }
                let key = match namespace {
                    Some(ns) => format!("{ns}:{}", s.name),
                    None => s.name.clone(),
                };
                out.insert(key, Arc::new(s));
            }
            Err(e) => tracing::warn!("skipping skill {}: {e:#}", skill_file.display()),
        }
    }
}

fn load_skill_file(path: &Path) -> Result<SkillDefinition> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = crate::frontmatter::parse::<SkillMeta>(&raw)
        .map_err(|e| anyhow::anyhow!("parsing skill {}: {e:#}", path.display()))?;
    Ok(SkillDefinition {
        name: parsed.front.name,
        description: parsed.front.description,
        body: parsed.body,
        source: path.to_path_buf(),
    })
}

static REGISTRY: OnceLock<SkillRegistry> = OnceLock::new();

pub fn init() {
    let registry = SkillRegistry::load();
    if let Err(existing) = REGISTRY.set(registry) {
        tracing::warn!(
            "skill registry already initialized ({} skills)",
            existing.list().len()
        );
    }
}

pub fn global() -> &'static SkillRegistry {
    REGISTRY.get().expect("skill registry not initialized")
}

/// Safe accessor for callers that may run before `init` (e.g. system-prompt
/// construction in tests): returns an empty summary when the registry is not
/// yet installed, so the prompt is well-formed throughout boot.
pub fn summary_block_or_empty() -> String {
    REGISTRY
        .get()
        .map(|r| r.summary_block())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_skill_frontmatter_and_body() {
        let raw = "---\nname: exam\ndescription: 生成试卷\n---\n# Exam\n工作流...\n";
        let f = crate::frontmatter::parse::<SkillMeta>(raw).unwrap();
        assert_eq!(f.front.name, "exam");
        assert_eq!(f.front.description, "生成试卷");
        assert!(f.body.contains("工作流"));
    }

    #[test]
    fn summary_block_empty_when_no_skills() {
        let r = SkillRegistry::default();
        assert!(r.summary_block().is_empty());
    }
}
