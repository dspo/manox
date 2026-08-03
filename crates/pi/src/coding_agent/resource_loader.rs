// Resource loading for the coding-agent facade: global (agentDir) and
// project instructions from the AGENTS/CLAUDE ancestor chain, skills, and
// prompt templates. Mirrors the TS discovery: per directory the candidates
// are checked in order `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD`
// (closest directory wins); the same file reached through different paths
// (symlinks, worktrees) is loaded once; skills are found recursively by
// `SKILL.md` plus direct root `.md` files and carry frontmatter name /
// description; name conflicts surface as diagnostics instead of silent
// overwrites.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::harness::{HarnessResources, PromptTemplate, Skill};

/// Loads resources for a working directory.
pub struct ResourceLoader {
    cwd: PathBuf,
    /// Global config directory whose instructions / skills / templates apply
    /// to every project.
    agent_dir: Option<PathBuf>,
}

/// A non-fatal discovery problem: name conflicts, unreadable files, or
/// malformed frontmatter. The load continues and the caller decides how to
/// surface the diagnostic.
#[derive(Debug, Clone)]
pub struct ResourceDiagnostic {
    pub message: String,
    pub path: String,
}

/// The result of a resource snapshot: the merged resources plus diagnostics.
#[derive(Debug, Clone)]
pub struct ResourceSnapshot {
    pub resources: HarnessResources,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Candidate instruction file names per directory, in TS preference order.
const INSTRUCTION_CANDIDATES: &[&str] = &["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

impl ResourceLoader {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        ResourceLoader {
            cwd: cwd.into(),
            agent_dir: None,
        }
    }

    /// Load the global (agentDir) instructions / skills / templates too.
    pub fn with_agent_dir(mut self, agent_dir: impl Into<PathBuf>) -> Self {
        self.agent_dir = Some(agent_dir.into());
        self
    }

    /// Snapshot the merged resources; diagnostics are discarded.
    pub async fn snapshot(&self) -> Result<HarnessResources, anyhow::Error> {
        Ok(self.snapshot_with_diagnostics().await?.resources)
    }

    /// Snapshot the merged resources plus discovery diagnostics.
    pub async fn snapshot_with_diagnostics(&self) -> Result<ResourceSnapshot, anyhow::Error> {
        let mut resources = HarnessResources::default();
        let mut diagnostics = Vec::new();
        let mut seen_files: HashSet<PathBuf> = HashSet::new();

        // Instructions: agentDir first, then the ancestor chain from cwd to
        // the filesystem root, closest directory first.
        if let Some(agent_dir) = &self.agent_dir {
            load_instruction_file(agent_dir, &mut resources, &mut diagnostics, &mut seen_files)
                .await;
        }
        let mut dir = Some(self.cwd.as_path());
        while let Some(current) = dir {
            load_instruction_file(current, &mut resources, &mut diagnostics, &mut seen_files).await;
            dir = current.parent();
        }

        // Skills: global first, then project — project wins name conflicts.
        if let Some(agent_dir) = &self.agent_dir {
            load_skills(agent_dir, &mut resources, &mut diagnostics).await;
        }
        load_skills(&self.cwd, &mut resources, &mut diagnostics).await;

        // Templates: global first, then project — project wins name conflicts.
        if let Some(agent_dir) = &self.agent_dir {
            load_templates(agent_dir, &mut resources, &mut diagnostics).await;
        }
        load_templates(&self.cwd, &mut resources, &mut diagnostics).await;

        Ok(ResourceSnapshot {
            resources,
            diagnostics,
        })
    }
}

/// Load the first instruction candidate of a directory as a skill, unless the
/// same file was already loaded through another path.
async fn load_instruction_file(
    dir: &Path,
    resources: &mut HarnessResources,
    diagnostics: &mut Vec<ResourceDiagnostic>,
    seen_files: &mut HashSet<PathBuf>,
) {
    let Some(candidate) = INSTRUCTION_CANDIDATES
        .iter()
        .find(|name| dir.join(name).is_file())
    else {
        return;
    };
    let path = dir.join(candidate);
    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
    if !seen_files.insert(canonical) {
        return;
    }
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("instructions")
                .to_lowercase();
            // Each ancestor level's instructions are distinct context; they
            // coexist even when they share a stem (multiple CLAUDE.md files).
            resources.skills.push(Skill {
                name: stem,
                description: format!("Instructions from {}", path.display()),
                location: path.to_string_lossy().into_owned(),
                content,
            });
        }
        Err(e) => diagnostics.push(ResourceDiagnostic {
            message: format!("failed to read instructions: {e}"),
            path: path.to_string_lossy().into_owned(),
        }),
    }
}

/// Discover skills under `root`: `SKILL.md` files anywhere (recursively,
/// named after their parent directory) plus direct root `.md` files.
async fn load_skills(
    root: &Path,
    resources: &mut HarnessResources,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) {
    let skills_dir = root.join("skills");
    if !skills_dir.is_dir() {
        return;
    }
    let mut stack = vec![skills_dir.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        let mut direct_md: Vec<PathBuf> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_some_and(|e| e == "md") {
                if path.file_name().is_some_and(|n| n == "SKILL.md") {
                    let name = dir
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("skill")
                        .to_string();
                    load_skill_file(&path, name, resources, diagnostics).await;
                } else if dir == skills_dir {
                    direct_md.push(path);
                }
            }
        }
        // Direct root .md files are skills named after their stem.
        direct_md.sort();
        for path in direct_md {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("skill")
                .to_string();
            load_skill_file(&path, name, resources, diagnostics).await;
        }
    }
}

/// Read a skill file, parse its frontmatter (`name` / `description`), and
/// merge it into the resources with a conflict diagnostic on name clash.
async fn load_skill_file(
    path: &Path,
    default_name: String,
    resources: &mut HarnessResources,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) {
    let Ok(content) = tokio::fs::read_to_string(path).await else {
        diagnostics.push(ResourceDiagnostic {
            message: "failed to read skill".into(),
            path: path.to_string_lossy().into_owned(),
        });
        return;
    };
    let (frontmatter, body) = parse_frontmatter(&content);
    let name = frontmatter
        .get("name")
        .cloned()
        .filter(|n| !n.is_empty())
        .unwrap_or(default_name);
    let description = frontmatter
        .get("description")
        .cloned()
        .filter(|d| !d.is_empty())
        .unwrap_or_default();
    push_skill(
        resources,
        diagnostics,
        Skill {
            name,
            description,
            location: path.to_string_lossy().into_owned(),
            content: body,
        },
    );
}

/// Merge a skill, recording a diagnostic when a same-named skill already
/// exists — the later (project) load wins the slot.
fn push_skill(
    resources: &mut HarnessResources,
    diagnostics: &mut Vec<ResourceDiagnostic>,
    skill: Skill,
) {
    if resources.skills.iter().any(|s| s.name == skill.name) {
        diagnostics.push(ResourceDiagnostic {
            message: format!(
                "skill name conflict: {:?} (project overrides global)",
                skill.name
            ),
            path: skill.location.clone(),
        });
        resources.skills.retain(|s| s.name != skill.name);
    }
    resources.skills.push(skill);
}

/// Load direct `templates/*.md` as prompt templates, project winning name
/// conflicts over global.
async fn load_templates(
    root: &Path,
    resources: &mut HarnessResources,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) {
    let templates_dir = root.join("templates");
    if !templates_dir.is_dir() {
        return;
    }
    let Ok(mut entries) = tokio::fs::read_dir(&templates_dir).await else {
        return;
    };
    let mut paths = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "md") {
            paths.push(path);
        }
    }
    paths.sort();
    for path in paths {
        let Ok(content) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("template")
            .to_string();
        if resources.prompt_templates.iter().any(|t| t.name == name) {
            diagnostics.push(ResourceDiagnostic {
                message: format!(
                    "prompt template name conflict: {name:?} (project overrides global)"
                ),
                path: path.to_string_lossy().into_owned(),
            });
            resources.prompt_templates.retain(|t| t.name != name);
        }
        resources
            .prompt_templates
            .push(PromptTemplate { name, content });
    }
}

/// Split frontmatter (`---`-delimited leading block) from the body, parsing
/// the few scalar fields the harness reads (`name`, `description`). Anything
/// more elaborate is ignored rather than failing the whole file.
fn parse_frontmatter(content: &str) -> (std::collections::HashMap<String, String>, String) {
    let normalized = content.replace("\r\n", "\n");
    if !normalized.starts_with("---") {
        return (std::collections::HashMap::new(), normalized);
    }
    let Some(end) = normalized.find("\n---") else {
        return (std::collections::HashMap::new(), normalized);
    };
    let raw = &normalized[3..end];
    let mut fields = std::collections::HashMap::new();
    for line in raw.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            if key == "name" || key == "description" {
                fields.insert(key.to_string(), value.trim().to_string());
            }
        }
    }
    (fields, normalized[end + 4..].trim_start().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn loads_instructions_from_candidates_closest_dir_wins() {
        let dir = tmp();
        let root = dir.path().join("proj");
        tokio::fs::create_dir_all(&root).await.unwrap();
        // Both candidates exist; AGENTS.md wins per the preference order.
        tokio::fs::write(root.join("AGENTS.md"), "agents instructions")
            .await
            .unwrap();
        tokio::fs::write(root.join("CLAUDE.md"), "claude instructions")
            .await
            .unwrap();

        let loader = ResourceLoader::new(&root);
        let snapshot = loader.snapshot_with_diagnostics().await.unwrap();
        let names: Vec<&str> = snapshot
            .resources
            .skills
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(names.contains(&"agents"), "{names:?}");
        assert!(!names.contains(&"claude"), "{names:?}");
    }

    #[tokio::test]
    async fn ancestor_chain_loads_each_level_once() {
        let dir = tmp();
        let root = dir.path().join("a").join("b").join("c");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(dir.path().join("a").join("CLAUDE.md"), "level a")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("a").join("b").join("CLAUDE.md"), "level b")
            .await
            .unwrap();
        tokio::fs::write(root.join("CLAUDE.md"), "level c")
            .await
            .unwrap();

        let loader = ResourceLoader::new(&root);
        let snapshot = loader.snapshot_with_diagnostics().await.unwrap();
        let claude_skills: Vec<&Skill> = snapshot
            .resources
            .skills
            .iter()
            .filter(|s| s.name == "claude")
            .collect();
        assert_eq!(claude_skills.len(), 3, "one per ancestor level");
        assert!(
            claude_skills.iter().any(|s| s.content.contains("level c")),
            "closest wins by position"
        );
    }

    #[tokio::test]
    async fn recursive_skill_discovery_with_frontmatter_and_conflicts() {
        let dir = tmp();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(cwd.join("skills").join("deep"))
            .await
            .unwrap();
        tokio::fs::write(
            cwd.join("skills/review.md"),
            "---\nname: review\ndescription: review the diff\n---\nCheck the diff.",
        )
        .await
        .unwrap();
        tokio::fs::write(
            cwd.join("skills/deep/SKILL.md"),
            "---\nname: deep-skill\n---\nDeep skill body.",
        )
        .await
        .unwrap();
        // A nested non-SKILL .md is ignored.
        tokio::fs::write(cwd.join("skills/deep/notes.md"), "ignored")
            .await
            .unwrap();

        let loader = ResourceLoader::new(&cwd);
        let snapshot = loader.snapshot_with_diagnostics().await.unwrap();
        let names: Vec<&str> = snapshot
            .resources
            .skills
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(names.contains(&"review"), "{names:?}");
        assert!(names.contains(&"deep-skill"), "{names:?}");
        let review = snapshot
            .resources
            .skills
            .iter()
            .find(|s| s.name == "review")
            .unwrap();
        assert_eq!(review.description, "review the diff");
        assert_eq!(review.content, "Check the diff.");
        assert!(
            !snapshot
                .resources
                .skills
                .iter()
                .any(|s| s.content.contains("ignored")),
            "nested non-SKILL md ignored"
        );

        // Same name twice -> conflict diagnostic, later load wins.
        let loader = ResourceLoader::new(&cwd);
        let snapshot = loader.snapshot_with_diagnostics().await.unwrap();
        assert_eq!(
            snapshot
                .resources
                .skills
                .iter()
                .filter(|s| s.name == "review")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn templates_load_with_project_winning_conflicts() {
        let dir = tmp();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(cwd.join("templates"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(agent_dir.join("templates"))
            .await
            .unwrap();
        tokio::fs::write(agent_dir.join("templates/fix.md"), "global fix")
            .await
            .unwrap();
        tokio::fs::write(cwd.join("templates/fix.md"), "project fix")
            .await
            .unwrap();

        let loader = ResourceLoader::new(&cwd).with_agent_dir(&agent_dir);
        let snapshot = loader.snapshot_with_diagnostics().await.unwrap();
        let fix = snapshot
            .resources
            .prompt_templates
            .iter()
            .find(|t| t.name == "fix")
            .unwrap();
        assert_eq!(fix.content, "project fix", "project wins the conflict");
        assert!(
            snapshot
                .diagnostics
                .iter()
                .any(|d| d.message.contains("name conflict")),
            "conflict recorded: {:?}",
            snapshot.diagnostics
        );
    }
}
