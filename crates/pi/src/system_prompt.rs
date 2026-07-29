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
/// project context.
pub fn build_system_prompt(
    base_instructions: Option<&str>,
    project: &ProjectContext,
) -> String {
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
        && !status.is_empty() {
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
            && let Ok(content) = std::fs::read_to_string(&path) {
                return Some(content);
            }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_system_prompt_basic() {
        let project = ProjectContext {
            cwd: "/home/user/project".into(),
            platform: "linux".into(),
            current_date: "2026-07-29".into(),
            ..Default::default()
        };

        let prompt = build_system_prompt(None, &project);
        assert!(prompt.contains("coding agent"));
        assert!(prompt.contains("/home/user/project"));
        assert!(prompt.contains("linux"));
        assert!(prompt.contains("2026-07-29"));
    }

    #[test]
    fn test_build_system_prompt_with_git() {
        let project = ProjectContext {
            cwd: "/project".into(),
            platform: "darwin".into(),
            git_branch: Some("feat/my-feature".into()),
            git_status: Some(" M src/main.rs".into()),
            current_date: "2026-07-29".into(),
            ..Default::default()
        };

        let prompt = build_system_prompt(None, &project);
        assert!(prompt.contains("feat/my-feature"));
        assert!(prompt.contains("src/main.rs"));
    }

    #[test]
    fn test_build_system_prompt_with_instructions() {
        let project = ProjectContext {
            cwd: "/project".into(),
            platform: "darwin".into(),
            project_instructions: Some("Always use async/await.".into()),
            current_date: "2026-07-29".into(),
            ..Default::default()
        };

        let prompt = build_system_prompt(None, &project);
        assert!(prompt.contains("Always use async/await"));
    }
}