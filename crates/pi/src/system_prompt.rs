// System prompt construction — builds the agent's system prompt with project
// context.
//
// The system prompt combines a base instruction template with dynamic
// project context: working directory, platform info, git status, and
// project-specific instructions from CLAUDE.md files.

use std::path::Path;

/// The base system prompt template for the coding agent.
const BASE_PROMPT: &str = "You are a coding agent — an AI assistant that helps with software \
    engineering tasks. You have access to tools for reading, writing, editing, and searching \
    files, as well as executing shell commands.\n\n\
    IMPORTANT: Follow the user's instructions carefully. When making changes, \
    prefer small, incremental edits over large rewrites. Always verify your \
    changes by reading the file before and after editing.";

/// Context gathered from the project environment.
#[derive(Debug, Clone, Default)]
pub struct ProjectContext {
    /// The current working directory.
    pub cwd: String,
    /// The operating system.
    pub platform: String,
    /// The current git branch, if any.
    pub git_branch: Option<String>,
    /// Recent git status summary.
    pub git_status: Option<String>,
    /// Content of CLAUDE.md (or similar project instructions), if found.
    pub project_instructions: Option<String>,
    /// The current date in ISO format.
    pub current_date: String,
}

/// Build the full system prompt by combining the base template with
/// project context. Kept as the context-rich variant; the facade uses
/// [`build_harness_prompt`] which folds context files and the skill index.
pub fn build_project_prompt(base_instructions: Option<&str>, project: &ProjectContext) -> String {
    let instructions = base_instructions.unwrap_or(BASE_PROMPT);

    let mut parts = vec![instructions.to_string()];

    // Project context section.
    let mut context = format!(
        "\n\n<project_context>\nWorking directory: {}\nPlatform: {}\nDate: {}",
        project.cwd, project.platform, project.current_date
    );

    if let Some(ref branch) = project.git_branch {
        context.push_str(&format!("\nGit branch: {branch}"));
    }

    if let Some(ref status) = project.git_status
        && !status.is_empty()
    {
        context.push_str(&format!("\nGit status:\n{status}"));
    }

    context.push_str("\n</project_context>");
    parts.push(context);

    // Project-specific instructions.
    if let Some(ref instructions) = project.project_instructions {
        parts.push(format!(
            "\n\n<project_instructions>\n{instructions}\n</project_instructions>"
        ));
    }

    parts.join("")
}

/// Try to load project instructions from common locations.
///
/// Looks for CLAUDE.md, .claude/instructions.md, AGENTS.md in the project root.
pub fn find_project_instructions(cwd: &Path) -> Option<String> {
    let candidates = [
        "CLAUDE.md",
        ".claude/instructions.md",
        "AGENTS.md",
        "AGENTS.txt",
    ];

    for candidate in &candidates {
        let path = cwd.join(candidate);
        if path.exists()
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            return Some(content);
        }
    }

    None
}

/// Build the harness system prompt from a base prompt, the project
/// instruction context files (AGENTS.md / CLAUDE.md), and the mounted skills.
/// This is the facade's single prompt builder — skills appear as an index so
/// the model knows what is invocable (TS `buildSystemPrompt`).
pub fn build_harness_prompt(
    base: &str,
    context_files: &[crate::harness::ContextFile],
    skills: &[crate::harness::Skill],
) -> String {
    let mut prompt = base.to_string();
    if !context_files.is_empty() {
        prompt.push_str("\n\n# Project instructions\n");
        for file in context_files {
            prompt.push_str(&format!("\n## {}\n\n{}\n", file.name, file.content));
        }
    }
    if !skills.is_empty() {
        prompt.push_str("\n# Available skills\n");
        for skill in skills {
            prompt.push_str(&format!(
                "- **{}**{}: {}\n",
                skill.name,
                if skill.description.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", skill.description)
                },
                skill.location
            ));
        }
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{ContextFile, Skill};

    /// The unified prompt builder folds context files and a skill index —
    /// the golden output the facade relies on.
    #[test]
    fn harness_prompt_golden() {
        let context = vec![ContextFile {
            name: "claude".into(),
            location: "/proj/CLAUDE.md".into(),
            content: "Keep changes minimal.".into(),
        }];
        let skills = vec![Skill {
            name: "review".into(),
            description: "review the diff".into(),
            location: "/proj/.pi/skills/review.md".into(),
            content: "Check the diff.".into(),
        }];
        let prompt = build_harness_prompt("Base prompt.", &context, &skills);
        assert_eq!(
            prompt,
            "Base prompt.\n\n# Project instructions\n\n## claude\n\nKeep changes minimal.\n\n# Available skills\n- **review** — review the diff: /proj/.pi/skills/review.md\n"
        );
    }

    #[test]
    fn harness_prompt_without_skills_omits_the_index() {
        let prompt = build_harness_prompt("Base.", &[], &[]);
        assert_eq!(prompt, "Base.");
    }
}
