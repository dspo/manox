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
//! `skills/<skill-name>/SKILL.md` is registered under the plugin's full
//! registry key, `name@marketplace:skill-name`, and user-authored
//! `~/.claude/skills/<name>/SKILL.md` files use the bare name. A plugin may
//! also carry a root-level `SKILL.md` (the plugin's own overview) —
//! registered under the key alone. The key, not the bare plugin name, is
//! the namespace: same-name installs from two marketplaces must stay
//! addressable instead of shadowing each other (#851).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::paths;
use crate::plugin::PluginManager;

#[derive(Debug, Clone, Deserialize)]
struct SkillMeta {
    /// Optional: when frontmatter omits it, the skill's directory name names
    /// the skill (the loaders' existing fallback).
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    /// Claude Code's `disable-model-invocation`: the skill stays
    /// user-invocable (slash) but is hidden from the model's summary list so
    /// the model neither discovers nor auto-invokes it. `allowed-tools` is
    /// accepted-and-ignored (the
    /// harness runs its full toolset for a turn).
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

/// A loaded skill: frontmatter identity + the body the `skill` tool returns.
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub body: String,
    pub disable_model_invocation: bool,
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
        // Plugin skills: namespaced by the plugin's full registry key
        // (`name@marketplace:skill`) + a bare-name root SKILL.md — the key,
        // not the bare name, keeps same-name installs from two marketplaces
        // from shadowing each other (#851).
        for plugin in PluginManager::installed() {
            let root = plugin.root.join("skills");
            if root.exists() {
                scan_skills_root(&root, Some(&plugin.key), &mut skills);
            }
            // A plugin-level root SKILL.md is the plugin's overview skill.
            let overview = plugin.root.join("SKILL.md");
            if overview.is_file()
                && let Ok(s) = load_skill_file(&overview)
            {
                skills.insert(plugin.key.clone(), Arc::new(s));
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
    /// (`name@marketplace:skill` or bare `skill`), distinct from
    /// `SkillDefinition::name` (the bare frontmatter name) — needed by
    /// callers that mirror skills into other registries keyed by the lookup
    /// form (e.g. the slash-command mirror).
    pub fn entries(&self) -> Vec<(&String, &Arc<SkillDefinition>)> {
        self.skills.iter().collect()
    }

    /// One-line `(name, description)` summaries for the system prompt, so the
    /// model knows which skills exist without their full bodies in context.
    /// Skills marked `disable-model-invocation` are withheld — the model must
    /// not discover them; users still invoke them via slash. The `system/main`
    /// template iterates this list — no markdown is built here.
    pub fn summaries(&self) -> Vec<crate::prompt::SkillSummaryPromptData> {
        self.skills
            .iter()
            .filter(|(_, s)| !s.disable_model_invocation)
            .map(|(key, s)| crate::prompt::SkillSummaryPromptData {
                name: key.clone(),
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
        disable_model_invocation: parsed.front.disable_model_invocation,
        source: path.to_path_buf(),
    })
}

/// Resolve one project-level skill on demand:
/// `<cwd>/.claude/skills/<name>/SKILL.md`. Project skills are cwd-scoped by
/// nature, so they are read fresh at use instead of living in the
/// process-global registry — a slash invocation from a thread whose cwd sits
/// in a repo with its own `.claude` picks that repo's skill up with no
/// restart. The *directory name is the identity*: it names the skill for
/// lookup and advertisement alike, so a frontmatter `name` that differs from
/// the directory can never advertise an unresolvable key (and the global
/// loader's frontmatter-name keying stays contained to its own layer).
pub fn resolve_project(cwd: &Path, name: &str) -> Option<SkillDefinition> {
    if name.is_empty()
        || name.contains(':')
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return None;
    }
    let skill_file = cwd
        .join(".claude")
        .join("skills")
        .join(name)
        .join("SKILL.md");
    let mut skill = load_skill_file(&skill_file).ok()?;
    skill.name = name.to_string();
    Some(skill)
}

/// Global summaries plus the project layer: `<cwd>/.claude/skills/`
/// entries shadow a global skill of the same bare name and honor
/// `disable-model-invocation`. Advertised under the directory name — the same
/// key `resolve_project` looks up — so advertise and resolve can never drift
/// apart. Built per session (the build knows the cwd), unlike the
/// process-global [`summaries`].
pub fn summaries_for_cwd(cwd: &Path) -> Vec<crate::prompt::SkillSummaryPromptData> {
    let mut out: Vec<crate::prompt::SkillSummaryPromptData> = summaries_or_empty();
    let dir = cwd.join(".claude").join("skills");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut project: Vec<crate::prompt::SkillSummaryPromptData> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let skill = resolve_project(cwd, &name)?;
            (!skill.disable_model_invocation).then_some(crate::prompt::SkillSummaryPromptData {
                name,
                description: skill.description,
            })
        })
        .collect();
    project.sort_by(|a, b| a.name.cmp(&b.name));
    for summary in project {
        out.retain(|s| s.name != summary.name);
        out.push(summary);
    }
    out
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
/// run before `manox_agent::init` (e.g. the UI slash-command registry init, which
/// `main` calls after `manox_agent::init` but is safer not to assume).
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

/// Write a user-authored skill to `~/.claude/skills/<name>/SKILL.md`.
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

    /// `summaries()` must advertise the full registry key (e.g.
    /// `name@marketplace:skill-name`), not the bare frontmatter `name`, so
    /// the model can pass it back to `get()` and resolve the skill. A bare
    /// name for a plugin skill would miss the lookup — the same bug class as
    /// plugin subagent types.
    #[test]
    fn summaries_use_registry_key_not_bare_name() {
        let mut skills = BTreeMap::new();
        // User skill: key == bare name.
        skills.insert(
            "exam".to_string(),
            Arc::new(SkillDefinition {
                name: "exam".to_string(),
                description: "生成试卷".to_string(),
                body: String::new(),
                disable_model_invocation: false,
                source: PathBuf::new(),
            }),
        );
        // Plugin skill: key has namespace prefix.
        skills.insert(
            "remora:task".to_string(),
            Arc::new(SkillDefinition {
                name: "task".to_string(),
                description: "delegate task".to_string(),
                body: String::new(),
                disable_model_invocation: false,
                source: PathBuf::new(),
            }),
        );
        let reg = SkillRegistry { skills };
        let sums = reg.summaries();

        // User skill: advertised name == key == bare name.
        let exam = sums
            .iter()
            .find(|s| s.name == "exam")
            .expect("exam summary");
        assert_eq!(exam.description, "生成试卷");

        // Plugin skill: advertised name == full key, NOT the bare frontmatter name.
        let remora = sums
            .iter()
            .find(|s| s.name == "remora:task")
            .expect("remora:task summary must use the full key");
        assert_eq!(remora.description, "delegate task");
        // The bare name "task" must NOT appear — it would be an unresolvable lookup.
        assert!(
            !sums.iter().any(|s| s.name == "task"),
            "bare plugin skill name must not be advertised — it is not a resolvable lookup key"
        );
    }

    #[test]
    fn resolve_project_keys_on_the_directory_name() {
        let proj = tempfile::tempdir().unwrap();
        let dir = proj.path().join(".claude").join("skills").join("tutor");
        std::fs::create_dir_all(&dir).unwrap();
        // Frontmatter omits `name`; the directory names the skill.
        std::fs::write(dir.join("SKILL.md"), "---\ndescription: d\n---\nbody").unwrap();
        let skill = resolve_project(proj.path(), "tutor").unwrap();
        assert_eq!(skill.name, "tutor");

        // Frontmatter `name` differing from the directory must not drift the
        // advertise key away from the lookup key: the directory wins.
        let dir2 = proj.path().join(".claude").join("skills").join("guide");
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(
            dir2.join("SKILL.md"),
            "---\nname: exam\ndescription: d\n---\nbody",
        )
        .unwrap();
        assert_eq!(resolve_project(proj.path(), "guide").unwrap().name, "guide");

        // Path-y names never resolve: `Path::join` with an absolute segment
        // would replace the base and escape `.claude/` entirely.
        assert!(resolve_project(proj.path(), "absent").is_none());
        assert!(resolve_project(proj.path(), "gitwork:review").is_none());
        assert!(resolve_project(proj.path(), "/etc/foo").is_none());
        assert!(resolve_project(proj.path(), "a/b").is_none());
    }

    #[test]
    fn summaries_hide_model_disabled_skills() {
        let mut skills = BTreeMap::new();
        skills.insert(
            "open".to_string(),
            Arc::new(SkillDefinition {
                name: "open".to_string(),
                description: "visible".to_string(),
                body: String::new(),
                disable_model_invocation: false,
                source: PathBuf::new(),
            }),
        );
        skills.insert(
            "secret".to_string(),
            Arc::new(SkillDefinition {
                name: "secret".to_string(),
                description: "user-only".to_string(),
                body: String::new(),
                disable_model_invocation: true,
                source: PathBuf::new(),
            }),
        );
        let reg = SkillRegistry { skills };
        let names: Vec<String> = reg.summaries().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["open"], "disable-model-invocation must be withheld");
    }

    #[test]
    fn project_summaries_shadow_global_same_name_and_hide_disabled() {
        let proj = tempfile::tempdir().unwrap();
        let exam = proj.path().join(".claude").join("skills").join("exam");
        std::fs::create_dir_all(&exam).unwrap();
        std::fs::write(
            exam.join("SKILL.md"),
            "---\nname: exam\ndescription: project-local exam\n---\nbody",
        )
        .unwrap();
        let secret = proj.path().join(".claude").join("skills").join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(
            secret.join("SKILL.md"),
            "---\nname: secret\ndescription: user-only\ndisable-model-invocation: true\n---\nbody",
        )
        .unwrap();

        // A frontmatter name differing from the directory is advertised under
        // the directory — the key lookups (and shadowing) actually use.
        let aliased = proj.path().join(".claude").join("skills").join("tutor");
        std::fs::create_dir_all(&aliased).unwrap();
        std::fs::write(
            aliased.join("SKILL.md"),
            "---\nname: frontmatter-name\ndescription: aliased\n---\nbody",
        )
        .unwrap();

        let merged = summaries_for_cwd(proj.path());
        let exam = merged
            .iter()
            .find(|s| s.name == "exam")
            .expect("exam present");
        assert_eq!(exam.description, "project-local exam");
        let tutor = merged
            .iter()
            .find(|s| s.name == "tutor")
            .expect("tutor present");
        assert_eq!(tutor.description, "aliased");
        assert!(
            !merged.iter().any(|s| s.name == "frontmatter-name"),
            "advertising the frontmatter name would advertise an unresolvable key"
        );
        assert!(
            !merged.iter().any(|s| s.name == "secret"),
            "disable-model-invocation project skills must not be advertised"
        );
    }
}
