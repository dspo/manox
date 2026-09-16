//! Built-in tool registry + shared helpers for the per-tool modules.
//!
//! Per-tool implementations live in sibling files (`read_file.rs`, `write_file.rs`,
//! `edit_file.rs`, `list_directory.rs`, `grep.rs`, `glob.rs`, `bash.rs`,
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

pub const ASK_USER_QUESTION: &str = "AskUserQuestion";
pub const BASH: &str = "Bash";
pub const BASH_OUTPUT: &str = "BashOutput";
pub const EDIT: &str = "Edit";
pub const GLOB: &str = "Glob";
pub const GREP: &str = "Grep";
pub const GET_GOAL: &str = "GetGoal";
pub const CREATE_GOAL: &str = "CreateGoal";
pub const MONITOR: &str = "Monitor";
pub const READ: &str = "Read";
pub const SKILL: &str = "Skill";
pub const UPDATE_PLAN: &str = "UpdatePlan";
pub const UPDATE_GOAL: &str = "UpdateGoal";
pub const WEB_FETCH: &str = "WebFetch";
pub const TASK_STOP: &str = "TaskStop";

pub const WRITE: &str = "Write";

// The manox harness tool implementations were removed with the retired
// manox harness; the constants above remain the shared wire-name source of
// truth.
