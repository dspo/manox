// Resource loading for the coding-agent facade: project instructions
// (CLAUDE.md), skills, and prompt templates are read from the working
// directory and surfaced as harness resources.

use crate::harness::{HarnessResources, PromptTemplate, Skill};

/// Loads resources for a working directory.
pub struct ResourceLoader {
    cwd: std::path::PathBuf,
}

impl ResourceLoader {
    pub fn new(cwd: impl Into<std::path::PathBuf>) -> Self {
        ResourceLoader { cwd: cwd.into() }
    }

    /// Snapshot the resources for the working directory: `CLAUDE.md` becomes
    /// a `project-instructions` skill, `skills/*.md` become skills, and
    /// `templates/*.md` become prompt templates. Missing files are skipped.
    pub async fn snapshot(&self) -> Result<HarnessResources, anyhow::Error> {
        let mut resources = HarnessResources::default();

        let instructions_path = self.cwd.join("CLAUDE.md");
        if instructions_path.is_file() {
            let content = tokio::fs::read_to_string(&instructions_path).await?;
            resources.skills.push(Skill {
                name: "project-instructions".into(),
                description: "Project instructions from CLAUDE.md".into(),
                location: instructions_path.to_string_lossy().into_owned(),
                content,
            });
        }

        let skills_dir = self.cwd.join("skills");
        if skills_dir.is_dir() {
            let mut entries = tokio::fs::read_dir(&skills_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md") {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("skill")
                        .to_string();
                    let content = tokio::fs::read_to_string(&path).await?;
                    resources.skills.push(Skill {
                        name,
                        description: "A project skill".into(),
                        location: path.to_string_lossy().into_owned(),
                        content,
                    });
                }
            }
        }

        let templates_dir = self.cwd.join("templates");
        if templates_dir.is_dir() {
            let mut entries = tokio::fs::read_dir(&templates_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md") {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("template")
                        .to_string();
                    let content = tokio::fs::read_to_string(&path).await?;
                    resources
                        .prompt_templates
                        .push(PromptTemplate { name, content });
                }
            }
        }

        Ok(resources)
    }
}
