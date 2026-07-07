//! Main-thread system prompt.
//!
//! Sub-agents carry their own `system` field loaded from `agents/*.md`; the
//! main thread has none (`Thread::system == None`), so this is minted fresh on
//! every request build — date changes, project may change — and prepended as a
//! `System` message by `Thread::build_completion_request`.
//!
//! The static body (working discipline + sandbox boundary) lives in
//! [`system_prompt.md`] next to this file, embedded via `include_str!` so the
//! prose reads as plain markdown and edits don't touch Rust — mirroring codex's
//! split (static `base_instructions/default.md` + dynamic environment block).
//! No template engine crate — string concatenation is enough.
//!
//! **Static-first layering for prefix-cache stability.** The prompt is
//! assembled most-static → most-volatile so the provider's prefix cache hits
//! the longest possible byte run turn-over-turn: (1) the compile-time static
//! prose + sandbox boundary, (2) the skills block (session-stable), (3) the
//! language directive (locale-stable), (4) the runtime identity block. Within
//! the identity block, session-stable rows (cwd/project/os/shell) come before
//! daily-volatile `today` and toggle-volatile `yolo`. Thread id is deliberately
//! NOT injected — the model fetches it on demand via the `self_info` tool
//! (codex and zed likewise do not inject session/thread id into the prompt).
//!
//! The prompt prose is fixed English regardless of the UI locale (the model's
//! context stays in one language). The user's preferred reply language is
//! conveyed by a one-line directive appended to the system prompt — see
//! [`build_main_system_prompt`] and [`language_directive`].

use std::path::Path;
use std::sync::OnceLock;

use crate::thread::ApprovalMode;

const STATIC_PROMPT: &str = include_str!("system_prompt.md");

/// Build the main-thread system prompt from live thread state.
///
/// Sub-agents never call this — their `system` field is `Some`, so
/// `build_completion_request` takes the `unwrap_or_else` branch only for the
/// main thread.
///
/// Assembly order is static-first (see module docs): the volatile identity
/// block lands at the very end so toggling `approval_mode` or a day rollover
/// only invalidates the cached tail, not the static prose. `PLAN_MODE_ADDENDUM`
/// (appended by `Thread::build_completion_request`) follows the identity block,
/// so toggling plan mode likewise only busts the tail.
pub fn build_main_system_prompt(
    cwd: &Path,
    project: Option<&Path>,
    approval_mode: ApprovalMode,
    active_worktree: Option<(&str, &Path)>,
) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let os = std::env::consts::OS;

    let mut prompt = String::from(STATIC_PROMPT);

    // Advertise installed skills so the model knows what reference docs it can
    // pull via the `skill` tool. The full bodies are loaded on demand, not
    // injected here — only the one-line summaries, to keep the prompt small.
    // Session-stable, so it sits above the volatile identity block.
    let skills = crate::skill::summary_block_or_empty();
    if !skills.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&skills);
    }

    // Locale-stable for the session; above the volatile identity block.
    prompt.push_str(language_directive());

    // Runtime identity — the only volatile section. Session-stable rows first
    // (cwd/project/os/shell), then the worktree row (toggle-volatile across
    // enter/exit), then daily-volatile `today`, then toggle-volatile `yolo`
    // last, so the cacheable prefix extends as far as possible.
    prompt.push_str("\n\n## Runtime identity\n");
    prompt.push_str(&format!(
        "- Current working directory: `{}`\n",
        cwd.display()
    ));
    if let Some(p) = project {
        prompt.push_str(&format!("- Project root: `{}`\n", p.display()));
    }
    if let Some((branch, path)) = active_worktree {
        prompt.push_str(&format!(
            "- Active worktree: `{branch}` at `{}`\n",
            path.display()
        ));
    }
    prompt.push_str(&format!("- Operating system: {os}\n"));
    prompt.push_str(&format!("- Default shell: {shell}\n"));
    let (python3, node) = runtime_versions();
    prompt.push_str(&format!("- python3: {python3}\n"));
    prompt.push_str(&format!("- node: {node}\n"));
    prompt.push_str(&format!("- Today: {today}\n"));
    match approval_mode {
        // OnRequest is the default; staying silent keeps the identity block
        // byte-stable for the common case. AutoReview and Yolo are the two
        // modes the model can act differently on, so only those are advertised
        // — and without revealing the internal mechanism (the reviewer LLM
        // exists, but the model doesn't need to know; adversarial framing
        // risk if it does).
        ApprovalMode::OnRequest => {}
        ApprovalMode::AutoReview => {
            prompt.push_str("- Mode: AutoReview (risky tool calls still ask before running)\n")
        }
        ApprovalMode::Yolo => prompt.push_str(
            "- Mode: YOLO (tool calls need no approval, bash runs outside the sandbox)\n",
        ),
    }

    prompt
}

/// Session-stable runtime versions for the identity block: `python3` and
/// `node` as reported by `<bin> --version` (first line), or `(absent)` when the
/// binary is missing. Probed once per process via a `OnceLock` so the prompt
/// stays byte-identical across requests (prefix-cache stable) and the spawn
/// cost is paid only on the first `build_main_system_prompt` call.
///
/// Motivated by thread 6cd3d096, where the model assumed Python 3.10+ and
/// emitted `match/case`, which `SyntaxError`'d on the actual 3.9.6 — the model
/// had no runtime facts to ground its version assumption.
fn runtime_versions() -> (&'static str, &'static str) {
    static VERSIONS: OnceLock<(String, String)> = OnceLock::new();
    let (py, node) = VERSIONS.get_or_init(|| (probe_version("python3"), probe_version("node")));
    (py.as_str(), node.as_str())
}

/// Capture the first line of `<bin> --version`'s stdout, or `(absent)` on any
/// failure (binary not on PATH, non-zero exit, non-UTF8). The model only needs
/// a best-effort label, not a strict parser.
fn probe_version(bin: &str) -> String {
    match std::process::Command::new(bin).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let full = String::from_utf8_lossy(&out.stdout);
            full.lines().next().unwrap_or("(absent)").trim().to_string()
        }
        _ => "(absent)".to_string(),
    }
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
        crate::i18n::Language::En => {
            "\n\n## Language\n\nUnless the user specifies otherwise, write your user-facing responses in English.\n"
        }
        crate::i18n::Language::ZhCn => {
            "\n\n## Language\n\nUnless the user specifies otherwise, write your user-facing responses in Simplified Chinese.\n"
        }
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
    format!(
        "You've reached the maximum turn count of {max}. Based on the work completed above, produce a concise final summary. Do not call any more tools."
    )
}

/// Appended to the system prompt while the thread is in plan mode. Tells the
/// model to delegate research to the read-only `plan`/`explore` sub-agents
/// (isolated context) and then submit its plan via `exit_plan_mode`. Kept in
/// code (not in `system_prompt.md`) for the same reason as
/// `max_turns_summary_prompt`: a short, templated instruction, not prose.
pub const PLAN_MODE_ADDENDUM: &str = "\n\n## Plan mode\n\
You are currently in plan mode: research the codebase and produce a plan, but do not implement.\n\
- You have read-only tools plus the `agent` tool. Delegate codebase research to the `plan` sub-agent (`agent` tool, `subagent_type=plan`) so the exploration stays in an isolated context and does not bloat this conversation. For a focused lookup (\"where is X defined\", \"which files reference Y\"), delegate to the `explore` sub-agent instead.\n\
- The sub-agent returns only its final conclusion; synthesize that into a complete plan. If research is inconclusive, delegate again with a sharper prompt rather than guessing.\n\
- Write tools and `bash` are hidden from you. Do not attempt to spawn write-capable sub-agents to bypass this — the bundled `plan`/`explore` are read-only by construction.\n\
- When the plan is ready, call `exit_plan_mode` with a step-by-step implementation plan: what each step changes, which existing functions to reuse, the tools each step will use, and any risks. End the plan with a `### Critical Files for Implementation` section listing 3–5 paths.\n\
- After you call `exit_plan_mode` the conversation pauses for user approval or rejection: approval exits plan mode and begins execution; rejection returns you to plan mode to revise the plan per the feedback — do not resubmit the same plan unchanged.\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_cwd_and_identity() {
        let cwd = Path::new("/tmp/some-proj");
        let p = build_main_system_prompt(cwd, None, ApprovalMode::OnRequest, None);
        assert!(p.contains("/tmp/some-proj"), "cwd must appear: {p}");
        assert!(p.contains("manox agent"), "identity must appear: {p}");
        assert!(p.contains("Today:"), "date row must appear: {p}");
        // Runtime versions are injected so the model does not guess (thread
        // 6cd3d096). The row is present regardless of whether the binary is
        // installed — absent binaries render as `(absent)`.
        assert!(p.contains("- python3:"), "python3 row must appear: {p}");
        assert!(p.contains("- node:"), "node row must appear: {p}");
    }

    #[test]
    fn prompt_does_not_leak_tech_stack() {
        // The identity names the product, not the implementation: the model has
        // no use for "GPUI"/"brush" or other framework names, and exposing
        // them only invites tangents.
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(!p.contains("GPUI"), "must not leak tech stack: {p}");
        assert!(!p.contains("gpui"), "must not leak tech stack: {p}");
        assert!(!p.contains("brush"), "must not leak tech stack: {p}");
    }

    #[test]
    fn runtime_identity_block_appended_at_tail() {
        // Identity is code-appended (no placeholder substitution), and it lands
        // at the very end of the prompt — after the static prose, skills, and
        // language directive — so the cacheable static prefix is maximal.
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(!p.contains("{{"), "no placeholder syntax: {p}");
        assert!(
            p.contains("## Runtime identity"),
            "identity block missing: {p}"
        );
        let identity_idx = p.find("## Runtime identity").expect("identity block");
        let sandbox_idx = p
            .find("## Tool sandbox boundary")
            .expect("sandbox boundary");
        // Identity must come after the sandbox boundary (static doc tail).
        assert!(
            identity_idx > sandbox_idx,
            "identity must follow the static sandbox boundary for cache stability: {p}"
        );
    }

    #[test]
    fn prompt_includes_context_economy() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(
            p.contains("Context economy"),
            "context economy section: {p}"
        );
        assert!(
            p.contains("byte-stable prefix"),
            "cache-awareness guidance: {p}"
        );
    }

    #[test]
    fn prompt_includes_project_when_set() {
        let cwd = Path::new("/tmp/some-proj");
        let proj = Path::new("/tmp/some-proj");
        let p = build_main_system_prompt(cwd, Some(proj), ApprovalMode::OnRequest, None);
        assert!(p.contains("Project root"));
    }

    #[test]
    fn prompt_contains_engineering_stance() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(
            p.contains("Engineering stance"),
            "engineering stance section: {p}"
        );
        assert!(
            p.contains("Own the end state"),
            "end-state responsibility: {p}"
        );
        assert!(p.contains("root cause"), "root-cause discipline: {p}");
    }

    #[test]
    fn prompt_contains_no_fabrication() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(
            p.contains("don't fabricate"),
            "no-fabrication discipline: {p}"
        );
    }

    #[test]
    fn prompt_contains_task_completion() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(
            p.contains("fully solved"),
            "task completion discipline: {p}"
        );
    }

    #[test]
    fn prompt_contains_validation_discipline() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(
            p.contains("Don't claim something passed without running it"),
            "validation discipline: {p}"
        );
    }

    #[test]
    fn prompt_contains_sandbox_boundary() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(
            p.contains("Tool sandbox boundary"),
            "sandbox boundary section: {p}"
        );
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
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(
            !p.contains("Current thread id"),
            "no thread id row in runtime identity block: {p}"
        );
    }

    #[test]
    fn static_prompt_is_embedded_verbatim() {
        // Editing the markdown must show through without rebuilding logic.
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(p.contains("Engineering stance"));
        assert!(p.contains("in-process native agent workbench"));
    }

    #[test]
    fn prompt_injects_language_directive() {
        // The current-locale language directive must land in the built prompt.
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(p.contains("## Language"), "language section missing: {p}");
        assert!(
            p.contains("write your user-facing responses in"),
            "language directive missing: {p}"
        );
    }

    #[test]
    fn yolo_mode_advertised_when_enabled() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::Yolo, None);
        assert!(p.contains("YOLO"), "yolo mode line missing: {p}");
    }

    #[test]
    fn yolo_mode_silent_when_disabled() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
        assert!(
            !p.contains("YOLO"),
            "yolo must not appear when disabled: {p}"
        );
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
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
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
        let p = build_main_system_prompt(Path::new("/tmp"), None, ApprovalMode::OnRequest, None);
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
