//! Built-in tool registry + shared helpers for the per-tool modules.
//!
//! Per-tool implementations live in sibling files (`read_file.rs`, `write_file.rs`,
//! `edit_file.rs`, `list_directory.rs`, `grep.rs`, `glob.rs`, `bash.rs`, `agent.rs`,
//! `ask_user.rs`, `monitor.rs`, `self_info.rs`, `skill.rs`). This module holds
//! the path/truncation helpers they share, plus the default registry assembly.
//!
//! `requires_approval` marks the tools the permission gate applies to:
//! mutating/remote calls are gated; reads stay open (see `pi_approval`).

// ─── tool name constants ────────────────────────────────────────────────────
//
// Single source of truth for every built-in tool's wire name. Each tool's
// `name()` returns its constant here, and every comparison site
// (`model_facing_content`, `tool_title`, truncation exemptions, etc.)
// references the same constant — a rename that misses a call site becomes a
// compile error instead of a silent runtime bug (see #273, #279).

pub const AGENT: &str = "Agent";
pub const ASK_USER_QUESTION: &str = "AskUserQuestion";
pub const BASH: &str = "Bash";
pub const BASH_OUTPUT: &str = "BashOutput";
pub const CODE: &str = "Code";
pub const EDIT: &str = "Edit";
pub const GLOB: &str = "Glob";
pub const GREP: &str = "Grep";
pub const GET_GOAL: &str = "GetGoal";
pub const CREATE_GOAL: &str = "CreateGoal";
pub const LIST: &str = "List";
pub const MONITOR: &str = "Monitor";
pub const READ: &str = "Read";
pub const SELF_INFO: &str = "SelfInfo";
pub const SKILL: &str = "Skill";
pub const TOOL_SEARCH: &str = "ToolSearch";
pub const UPDATE_PLAN: &str = "UpdatePlan";
pub const UPDATE_GOAL: &str = "UpdateGoal";
pub const WEB_FETCH: &str = "WebFetch";
pub const ENTER_WORKTREE: &str = "EnterWorktree";
pub const EXIT_WORKTREE: &str = "ExitWorktree";
pub const TASK_STOP: &str = "TaskStop";

pub const WRITE: &str = "Write";

/// One-line observation title for a spawned sub-agent's task prompt:
/// whitespace-flattened and capped at 60 chars with an ellipsis. Single
/// source for both the rail's `latest_activity` and the conversation's Agent
/// task rows, so every surface shows the same topic.
pub fn subagent_topic(prompt: &str) -> String {
    let flat: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flat.chars();
    let head: String = chars.by_ref().take(60).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

// The manox harness tool implementations were removed with the retired
// manox harness; the constants above remain the shared wire-name source of
// truth.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_topic_flattens_and_caps() {
        assert_eq!(
            subagent_topic("  find   the\nauth module "),
            "find the auth module"
        );
        let long = "x ".repeat(40); // 80 chars
        let topic = subagent_topic(&long);
        assert!(topic.ends_with('…'));
        assert_eq!(topic.chars().count(), 61);
    }

    #[test]
    fn subagent_topic_short_prompt_unchanged() {
        assert_eq!(subagent_topic("review PR #123"), "review PR #123");
    }
}
