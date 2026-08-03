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
/// The default coding-agent base prompt, ported from the TS
/// `buildSystemPrompt` non-UI core: the identity line and the always-present
/// guidelines. The UI documentation section and per-tool snippets are
/// dynamic (added by [`build_harness_prompt`]).
pub const DEFAULT_BASE_PROMPT: &str = "You are an expert coding assistant operating inside pi, a coding agent harness. \
    You help users by reading files, executing commands, editing code, and writing new files.\n\n\
    In addition to the tools below, you may have access to other custom tools depending on the project.\n\n\
    Guidelines:\n\
    - Be concise in your responses\n\
    - Show file paths clearly when working with files";

/// One-line tool snippets for the Available tools list (TS `toolSnippets`).
const TOOL_SNIPPETS: &[(&str, &str)] = &[
    ("read", "Read a file from the filesystem"),
    ("bash", "Run a shell command"),
    ("edit", "Edit a file with a patch"),
    ("write", "Write a file"),
    ("grep", "Search file contents"),
    ("find", "Locate files"),
    ("ls", "List directory contents"),
];

pub fn build_harness_prompt(
    base: &str,
    cwd: &std::path::Path,
    active_tools: &[String],
    context_files: &[crate::harness::ContextFile],
    skills: &[crate::harness::Skill],
) -> String {
    let mut prompt = base.to_string();
    prompt.push_str("\n\nAvailable tools:\n");
    if active_tools.is_empty() {
        prompt.push_str("(none)\n");
    } else {
        for name in active_tools {
            let snippet = TOOL_SNIPPETS
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, s)| *s)
                .unwrap_or("Available");
            prompt.push_str(&format!("- {name}: {snippet}\n"));
        }
    }
    if !context_files.is_empty() {
        prompt.push_str("\n<project_context>\n\nProject-specific instructions and guidelines:\n\n");
        for file in context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                xml_escape(&file.location),
                file.content
            ));
        }
        prompt.push_str("</project_context>\n");
    }
    let has_read = active_tools.iter().any(|t| t == "read");
    if has_read && !skills.is_empty() {
        prompt.push_str(
            "\nSkills are markdown files you can read for detailed instructions; \
             use the read tool on a skill's path before relying on it.\n",
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
    prompt.push_str(&format!("\nCurrent working directory: {}\n", cwd.display()));
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
            DEFAULT_BASE_PROMPT,
            std::path::Path::new("/proj"),
            &tools,
            &context,
            &skills,
        );
        assert!(prompt.contains("Current working directory: /proj"));
        assert!(prompt.contains("- read: Read a file from the filesystem"));
        assert!(prompt.contains("- bash: Run a shell command"));
        assert!(prompt.contains(
            "<project_instructions path=\"/proj/CLAUDE.md\">\nKeep changes minimal.\n</project_instructions>"
        ));
        assert!(prompt.contains(
            "<skill name=\"review\">\n<description>review the diff</description>\n<path>/proj/.pi/skills/review.md</path>\n</skill>"
        ));
        assert!(prompt.contains("Be concise in your responses"));
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
