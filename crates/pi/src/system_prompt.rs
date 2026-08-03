// System prompt construction — builds the agent's system prompt with project
// context.
//
// The system prompt combines a base instruction template with dynamic
// project context: working directory, platform info, git status, and
// project-specific instructions from CLAUDE.md files.

/// Build the harness system prompt from a base prompt, the project
/// instruction context files (AGENTS.md / CLAUDE.md), and the mounted skills.
/// This is the facade's single prompt builder — skills appear as an index so
/// the model knows what is invocable (TS `buildSystemPrompt`).
/// The default coding-agent base prompt: identity, active-tool guidance,
/// and editing guidelines (the UI-documentation sections of the TS default
/// prompt are intentionally out of scope).
pub const DEFAULT_BASE_PROMPT: &str = "You are a coding agent — an AI assistant that helps with software \
    engineering tasks in the working directory below. You have access to tools for reading, \
    writing, and editing files, running shell commands, and searching code.\n\n\
    Guidelines:\n\
    - Follow the user's instructions carefully.\n\
    - When making changes, prefer small, incremental edits over large rewrites.\n\
    - Verify your changes by reading the file before and after editing.\n\
    - Preserve exact file paths, function names, and error messages.\n\
    - Rely on actual tool results; never fabricate file contents or command output.\n\
    - When a task is ambiguous, ask for clarification instead of guessing.";

/// Build the harness system prompt from the base prompt, the working
/// directory, the active tool set, the project instruction context files
/// (AGENTS.md / CLAUDE.md, each with its real path), and the mounted skills
/// as invocable XML blocks with read guidance — the facade's single prompt
/// builder. Skills are hidden when no `read` tool is mounted (they cannot
/// be referenced usefully).
pub fn build_harness_prompt(
    base: &str,
    cwd: &std::path::Path,
    active_tools: &[String],
    context_files: &[crate::harness::ContextFile],
    skills: &[crate::harness::Skill],
) -> String {
    let mut prompt = base.to_string();
    prompt.push_str(&format!("\n\nWorking directory: {}\n", cwd.display()));
    if !active_tools.is_empty() {
        prompt.push_str(&format!("\nAvailable tools: {}\n", active_tools.join(", ")));
    }
    if !context_files.is_empty() {
        prompt.push_str("\n# Project instructions\n");
        for file in context_files {
            prompt.push_str(&format!(
                "\n<project_instructions path=\"{}\">\n{}\n</project_instructions>\n",
                file.location, file.content
            ));
        }
    }
    let has_read = active_tools.iter().any(|t| t == "read");
    if has_read && !skills.is_empty() {
        prompt.push_str("\n# Skills\n");
        prompt.push_str(
            "Skills are markdown files you can read for detailed instructions.              Use the read tool on a skill's path before relying on it.\n",
        );
        for skill in skills {
            prompt.push_str(&format!(
                "<skill name=\"{}\">\n<description>{}</description>\n<path>{}</path>\n</skill>\n",
                xml_escape(&skill.name),
                xml_escape(&skill.description),
                xml_escape(&skill.location)
            ));
        }
    }
    prompt
}

/// Escape XML-special characters in prompt-embedded strings.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
        let tools = vec!["read".to_string(), "bash".to_string()];
        let prompt = build_harness_prompt(
            "Base prompt.",
            std::path::Path::new("/proj"),
            &tools,
            &context,
            &skills,
        );
        assert!(prompt.contains("Working directory: /proj"));
        assert!(prompt.contains("Available tools: read, bash"));
        assert!(prompt.contains(
            "<project_instructions path=\"/proj/CLAUDE.md\">\nKeep changes minimal.\n</project_instructions>"
        ));
        assert!(prompt.contains(
            "<skill name=\"review\">\n<description>review the diff</description>\n<path>/proj/.pi/skills/review.md</path>\n</skill>"
        ));
    }

    #[test]
    fn harness_prompt_hides_skills_without_a_read_tool() {
        let skills = vec![Skill {
            name: "review".into(),
            description: String::new(),
            location: "/proj/.pi/skills/review.md".into(),
            content: "x".into(),
        }];
        // No read tool: skills are hidden — they cannot be referenced.
        let prompt =
            build_harness_prompt("Base.", std::path::Path::new("/proj"), &[], &[], &skills);
        assert!(!prompt.contains("<skill"), "{prompt}");
    }
}
