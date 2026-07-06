//! Main-thread system prompt.
//!
//! Sub-agents carry their own `system` field loaded from `agents/*.md`; the
//! main thread has none (`Thread::system == None`), so this is minted fresh on
//! every request build — date changes, project may change — and prepended as a
//! `System` message by `Thread::build_completion_request`.
//!
//! The static body (identity + working discipline + sandbox boundary) lives in
//! [`system_prompt.md`] next to this file, embedded via `include_str!` so the
//! prose reads as plain markdown and edits don't touch Rust — mirroring codex's
//! split (static `base_instructions/default.md` + dynamic `<cwd>`/
//! `<current_date>` environment XML). The markdown carries a
//! `{{runtime_identity}}` placeholder where the live block belongs; this
//! builder renders the cwd/project/os/shell/date key-value rows into it. No
//! template engine crate — `str::replace` is enough for one tag. Thread id is
//! deliberately NOT injected — the model fetches it on demand via the
//! `self_info` tool (codex and zed likewise do not inject session/thread id
//! into the prompt).
//!
//! The prompt prose is fixed English regardless of the UI locale (the model's
//! context stays in one language). The user's preferred reply language is
//! conveyed by a one-line directive injected into the runtime identity block —
//! see [`build_main_system_prompt`] and [`language_directive`].

use std::path::Path;

const STATIC_PROMPT: &str = include_str!("system_prompt.md");

/// Tag in `system_prompt.md` replaced with the live runtime identity block.
const RUNTIME_IDENTITY_TAG: &str = "{{runtime_identity}}";

/// Build the main-thread system prompt from live thread state.
///
/// Sub-agents never call this — their `system` field is `Some`, so
/// `build_completion_request` takes the `unwrap_or_else` branch only for the
/// main thread.
pub fn build_main_system_prompt(cwd: &Path, project: Option<&Path>, yolo: bool) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let os = std::env::consts::OS;

    let mut identity = String::from("## Runtime identity\n");
    identity.push_str(&format!("- Current working directory: `{}`\n", cwd.display()));
    if let Some(p) = project {
        identity.push_str(&format!("- Project root: `{}`\n", p.display()));
    }
    identity.push_str(&format!("- Operating system: {os}\n"));
    identity.push_str(&format!("- Default shell: {shell}\n"));
    identity.push_str(&format!("- Today: {today}\n"));
    if yolo {
        identity.push_str("- Mode: YOLO (tool calls need no approval, bash runs outside the sandbox)\n");
    }
    identity.push_str(language_directive());

    let mut prompt = STATIC_PROMPT.replace(RUNTIME_IDENTITY_TAG, &identity);
    // Advertise installed skills so the model knows what reference docs it can
    // pull via the `skill` tool. The full bodies are loaded on demand, not
    // injected here — only the one-line summaries, to keep the prompt small.
    let skills = crate::skill::summary_block_or_empty();
    if !skills.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&skills);
    }
    prompt
}

/// The language directive injected into the system prompt so the model addresses
/// the user in the UI's chosen language. The prompt prose itself stays English;
/// only this one directive varies with [`crate::i18n::current`]. Appendable to
/// a sub-agent's `system` string as well, to keep sub-agent reply language
/// consistent with the main thread.
pub fn language_directive() -> &'static str {
    // The name is always an English endonym ("English", "Simplified Chinese") —
    // the model parses the directive, the user never sees this string.
    match crate::i18n::current() {
        crate::i18n::Language::En => "\n\n## Language\n\nUnless the user specifies otherwise, write your user-facing responses in English.\n",
        crate::i18n::Language::ZhCn => "\n\n## Language\n\nUnless the user specifies otherwise, write your user-facing responses in Simplified Chinese.\n",
    }
}

/// The user message injected when a sub-agent hits its `max_turns` cap.
///
/// Kept here rather than in `system_prompt.md` because it is a one-line
/// template, not prose — codex and zed likewise keep short turn-cap templates
/// in code. The first cap hit asks for a coherent final summary; a second cap
/// hit (the sub-agent keeps calling tools) is what actually hard-stops the turn
/// in `Thread::run_turn_loop`.
pub fn max_turns_summary_prompt(max: u32) -> String {
    format!("You've reached the maximum turn count of {max}. Based on the work completed above, produce a concise final summary. Do not call any more tools.")
}

/// Appended to the system prompt while the thread is in plan mode. Tells the
/// model the read-only constraint and how to exit via `exit_plan_mode`. Kept in
/// code (not in `system_prompt.md`) for the same reason as
/// `max_turns_summary_prompt`: a short, templated instruction, not prose.
pub const PLAN_MODE_ADDENDUM: &str = "\n\n## Plan mode\n\
You are currently in plan mode.\n\
- You may only use read-only tools (read_file / list_directory / grep / glob / AskUserQuestion / self_info / skill) to research the codebase.\n\
- Write tools, bash execution, and the sub-agent spawning tool are hidden from you; do not attempt them.\n\
- After thorough research, call the `exit_plan_mode` tool to submit your plan: it should include a step-by-step implementation plan, the tools each step will use, and any potential risks.\n\
- After you call `exit_plan_mode` the conversation pauses for user approval or rejection: approval exits plan mode and begins execution; rejection returns you to plan mode to revise the plan per the feedback — do not resubmit the same plan unchanged.\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_cwd_and_identity() {
        let cwd = Path::new("/tmp/some-proj");
        let p = build_main_system_prompt(cwd, None, false);
        assert!(p.contains("/tmp/some-proj"), "cwd must appear: {p}");
        assert!(p.contains("manox agent"), "identity must appear: {p}");
        assert!(p.contains("Today:"), "date row must appear: {p}");
    }

    #[test]
    fn prompt_does_not_leak_tech_stack() {
        // The identity names the product, not the implementation: the model has
        // no use for "GPUI"/"brush" or other framework names, and exposing
        // them only invites tangents.
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(!p.contains("GPUI"), "must not leak tech stack: {p}");
        assert!(!p.contains("gpui"), "must not leak tech stack: {p}");
        assert!(!p.contains("brush"), "must not leak tech stack: {p}");
    }

    #[test]
    fn runtime_identity_placeholder_is_rendered() {
        // The {{runtime_identity}} tag must be substituted with live values,
        // never reach the model as a literal placeholder.
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(!p.contains("{{"), "placeholder leaked: {p}");
        assert!(!p.contains("runtime_identity}}"), "placeholder leaked: {p}");
        assert!(p.contains("## Runtime identity"), "identity block missing: {p}");
    }

    #[test]
    fn prompt_includes_project_when_set() {
        let cwd = Path::new("/tmp/some-proj");
        let proj = Path::new("/tmp/some-proj");
        let p = build_main_system_prompt(cwd, Some(proj), false);
        assert!(p.contains("Project root"));
    }

    #[test]
    fn prompt_contains_engineering_stance() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(p.contains("Engineering stance"), "engineering stance section: {p}");
        assert!(p.contains("Own the end state"), "end-state responsibility: {p}");
        assert!(p.contains("root cause"), "root-cause discipline: {p}");
    }

    #[test]
    fn prompt_contains_no_fabrication() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(p.contains("don't fabricate"), "no-fabrication discipline: {p}");
    }

    #[test]
    fn prompt_contains_task_completion() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(p.contains("fully solved"), "task completion discipline: {p}");
    }

    #[test]
    fn prompt_contains_validation_discipline() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(
            p.contains("Don't claim something passed without running it"),
            "validation discipline: {p}"
        );
    }

    #[test]
    fn prompt_contains_sandbox_boundary() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(p.contains("Tool sandbox boundary"), "sandbox boundary section: {p}");
        assert!(
            p.contains("`.git` directory is read-only"),
            ".git protected: {p}"
        );
        assert!(
            p.contains("unsandboxed"),
            "unsandboxed knob documented: {p}"
        );
    }

    #[test]
    fn prompt_does_not_inject_thread_id() {
        // Thread id is fetched via the self_info tool, never injected into the
        // prompt. The runtime identity block must not carry a thread id row —
        // the prose may mention "thread id" as a concept pointing to the tool,
        // but no concrete id value is injected here.
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(
            !p.contains("Current thread id"),
            "no thread id row in runtime identity block: {p}"
        );
    }

    #[test]
    fn static_prompt_is_embedded_verbatim() {
        // Editing the markdown must show through without rebuilding logic.
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(p.contains("Engineering stance"));
        assert!(p.contains("in-process native agent workbench"));
    }

    #[test]
    fn prompt_injects_language_directive() {
        // The current-locale language directive must land in the built prompt.
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(p.contains("## Language"), "language section missing: {p}");
        assert!(
            p.contains("write your user-facing responses in"),
            "language directive missing: {p}"
        );
    }

    #[test]
    fn yolo_mode_advertised_when_enabled() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, true);
        assert!(p.contains("YOLO"), "yolo mode line missing: {p}");
    }

    #[test]
    fn yolo_mode_silent_when_disabled() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(!p.contains("YOLO"), "yolo must not appear when disabled: {p}");
    }

    #[test]
    fn max_turns_summary_prompt_contains_cap_and_no_tools() {
        let s = max_turns_summary_prompt(10);
        assert!(s.contains("10"), "cap value must appear: {s}");
        assert!(
            s.contains("Do not call any more tools"),
            "no-tools directive: {s}"
        );
    }

    #[test]
    fn prompt_distinguishes_discussion_from_implementation() {
        // "How do I X" is a discussion request, not an implementation request —
        // the agent must answer first and ask before touching code (thread
        // bfb39601: agent started implementing on a "how do I add a
        // marketplace" question).
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(
            p.contains("Discussion vs implementation"),
            "discussion section: {p}"
        );
        assert!(p.contains("discussion or Q&A"), "discussion framing: {p}");
        assert!(
            p.contains("shall I implement this now?"),
            "ask-before-implementing: {p}"
        );
        assert!(
            p.contains("Don't modify code without an explicit request"),
            "no-unsolicited-code-changes: {p}"
        );
        assert!(
            p.contains("the user's actual request"),
            "task-execution scoped to actual request: {p}"
        );
    }

    #[test]
    fn prompt_contains_git_verification_discipline() {
        // Every git write op must be verified with git itself before reporting
        // success — the branch-tracking false-success regression (thread
        // e5047fd2) came from reporting push success without checking
        // `git log origin/<branch>`.
        let p = build_main_system_prompt(Path::new("/tmp"), None, false);
        assert!(p.contains("Git operations"), "git section: {p}");
        assert!(
            p.contains("git log origin/<branch>"),
            "remote verification: {p}"
        );
        assert!(
            p.contains("Don't report success without verifying"),
            "no-success-without-verify: {p}"
        );
        assert!(
            p.contains("git branch --show-current"),
            "branch name from measurement: {p}"
        );
    }
}
