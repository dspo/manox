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

#[derive(Debug, Clone)]
pub enum SkillOrigin {
    User,
    Plugin { plugin: String },
}

#[derive(Debug, Clone)]
pub struct SkillRecord {
    pub key: String,
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: PathBuf,
    pub origin: SkillOrigin,
}

#[derive(Debug, Clone)]
pub struct UserSkillDraft {
    pub name: String,
    pub description: String,
    pub body: String,
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

    /// `(registry_key, definition)` pairs. The key is the full lookup name
    /// (`plugin:skill` or bare `skill`), distinct from `SkillDefinition::name`
    /// (the bare frontmatter name) — needed by callers that mirror skills into
    /// other registries keyed by the lookup form (e.g. the slash-command mirror).
    pub fn entries(&self) -> Vec<(&String, &Arc<SkillDefinition>)> {
        self.skills.iter().collect()
    }

    /// One-line `(name, description)` summaries for the system prompt, so the
    /// model knows which skills exist without their full bodies in context.
    /// The `system/main` template iterates this list — no markdown is built here.
    pub fn summaries(&self) -> Vec<crate::prompt::SkillSummaryPromptData> {
        self.skills
            .values()
            .map(|s| crate::prompt::SkillSummaryPromptData {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect()
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

/// Non-panicking accessor mirroring `command::try_global`, for callers that may
/// run before `agent::init` (e.g. the UI slash-command registry init, which
/// `main` calls after `agent::init` but is safer not to assume).
pub fn try_global() -> Option<&'static SkillRegistry> {
    REGISTRY.get()
}

/// Freshly scan the filesystem and return a UI-friendly list of skills,
/// including user-authored and plugin-provided entries. This bypasses the
/// process-global registry so management views can reflect changes made during
/// the current app session.
pub fn list_skill_records() -> Vec<SkillRecord> {
    let registry = SkillRegistry::load();
    let user_root = paths::skills_dir().ok();
    registry
        .skills
        .iter()
        .map(|(key, skill)| SkillRecord {
            key: key.clone(),
            name: skill.name.clone(),
            description: skill.description.clone(),
            body: skill.body.clone(),
            source: skill.source.clone(),
            origin: classify_origin(key, &skill.source, user_root.as_deref()),
        })
        .collect()
}

/// Write a user-authored skill to `~/.config/cx/manox/skills/<name>/SKILL.md`.
/// When `previous_name` differs from `draft.name`, the old directory is removed
/// after the new one is written so renames do not leave stale copies behind.
pub fn save_user_skill(draft: &UserSkillDraft, previous_name: Option<&str>) -> Result<()> {
    let name = validate_user_skill_name(&draft.name)?;
    let root = paths::skills_dir()?.join(&name);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("creating skill dir {}", root.display()))?;
    let path = root.join("SKILL.md");
    #[derive(serde::Serialize)]
    struct Frontmatter<'a> {
        name: &'a str,
        description: &'a str,
    }
    let front = serde_yaml::to_string(&Frontmatter {
        name: &name,
        description: &draft.description,
    })
    .context("serializing skill frontmatter")?;
    let mut doc = String::from("---\n");
    doc.push_str(&front);
    doc.push_str("---\n");
    doc.push_str(&draft.body);
    if !doc.ends_with('\n') {
        doc.push('\n');
    }
    std::fs::write(&path, doc).with_context(|| format!("writing {}", path.display()))?;

    if let Some(previous) = previous_name {
        let previous = previous.trim();
        if !previous.is_empty() && previous != name {
            let old_root = paths::skills_dir()?.join(previous);
            if old_root.exists() {
                std::fs::remove_dir_all(&old_root)
                    .with_context(|| format!("removing old skill dir {}", old_root.display()))?;
            }
        }
    }
    Ok(())
}

pub fn remove_user_skill(name: &str) -> Result<()> {
    let name = validate_user_skill_name(name)?;
    let root = paths::skills_dir()?.join(name);
    if root.exists() {
        std::fs::remove_dir_all(&root)
            .with_context(|| format!("removing skill dir {}", root.display()))?;
    }
    Ok(())
}

/// Safe accessor for callers that may run before `init` (e.g. system-prompt
/// construction in tests): returns an empty list when the registry is not yet
/// installed, so the prompt is well-formed throughout boot.
pub fn summaries_or_empty() -> Vec<crate::prompt::SkillSummaryPromptData> {
    REGISTRY.get().map(|r| r.summaries()).unwrap_or_default()
}

fn classify_origin(key: &str, source: &Path, user_root: Option<&Path>) -> SkillOrigin {
    if let Some(root) = user_root
        && source.starts_with(root)
    {
        return SkillOrigin::User;
    }
    let plugin = key
        .split_once(':')
        .map(|(plugin, _)| plugin.to_string())
        .or_else(|| {
            source
                .components()
                .rev()
                .nth(2)
                .map(|part| part.as_os_str().to_string_lossy().to_string())
        })
        .unwrap_or_default();
    SkillOrigin::Plugin { plugin }
}

fn validate_user_skill_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("skill name cannot be empty");
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed == "." || trimmed == ".." {
        anyhow::bail!("skill name contains an invalid path segment");
    }
    Ok(trimmed.to_string())
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
    fn summaries_empty_when_no_skills() {
        let r = SkillRegistry::default();
        assert!(r.summaries().is_empty());
    }
}
