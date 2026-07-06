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

use std::path::Path;

const STATIC_PROMPT: &str = include_str!("system_prompt.md");

/// Tag in `system_prompt.md` replaced with the live runtime identity block.
const RUNTIME_IDENTITY_TAG: &str = "{{runtime_identity}}";

/// Build the main-thread system prompt from live thread state.
///
/// Sub-agents never call this — their `system` field is `Some`, so
/// `build_completion_request` takes the `unwrap_or_else` branch only for the
/// main thread.
pub fn build_main_system_prompt(cwd: &Path, project: Option<&Path>) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let os = std::env::consts::OS;

    let mut identity = String::from("## 运行时身份\n");
    identity.push_str(&format!("- 当前工作目录：`{}`\n", cwd.display()));
    if let Some(p) = project {
        identity.push_str(&format!("- 项目根：`{}`\n", p.display()));
    }
    identity.push_str(&format!("- 操作系统：{os}\n"));
    identity.push_str(&format!("- 默认 shell：{shell}\n"));
    identity.push_str(&format!("- 今天：{today}\n"));

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

/// The user message injected when a sub-agent hits its `max_turns` cap.
///
/// Kept here rather than in `system_prompt.md` because it is a one-line
/// template, not prose — codex and zed likewise keep short turn-cap templates
/// in code. The first cap hit asks for a coherent final summary; a second cap
/// hit (the sub-agent keeps calling tools) is what actually hard-stops the turn
/// in `Thread::run_turn_loop`.
pub fn max_turns_summary_prompt(max: u32) -> String {
    format!(
        "你已达到最大轮次 {max}。请基于上述已完成的工作，给出一个简洁的最终总结，不要再调用任何工具。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_cwd_and_identity() {
        let cwd = Path::new("/tmp/some-proj");
        let p = build_main_system_prompt(cwd, None);
        assert!(p.contains("/tmp/some-proj"), "cwd must appear: {p}");
        assert!(p.contains("manox agent"), "identity must appear: {p}");
        assert!(p.contains("今天"), "date must appear: {p}");
    }

    #[test]
    fn prompt_does_not_leak_tech_stack() {
        // The identity names the product, not the implementation: the model has
        // no use for "GPUI"/"brush" or other framework names, and exposing
        // them only invites tangents.
        let p = build_main_system_prompt(Path::new("/tmp"), None);
        assert!(!p.contains("GPUI"), "must not leak tech stack: {p}");
        assert!(!p.contains("gpui"), "must not leak tech stack: {p}");
        assert!(!p.contains("brush"), "must not leak tech stack: {p}");
    }

    #[test]
    fn runtime_identity_placeholder_is_rendered() {
        // The {{runtime_identity}} tag must be substituted with live values,
        // never reach the model as a literal placeholder.
        let p = build_main_system_prompt(Path::new("/tmp"), None);
        assert!(!p.contains("{{"), "placeholder leaked: {p}");
        assert!(!p.contains("runtime_identity}}"), "placeholder leaked: {p}");
        assert!(p.contains("## 运行时身份"), "identity block missing: {p}");
    }

    #[test]
    fn prompt_includes_project_when_set() {
        let cwd = Path::new("/tmp/some-proj");
        let proj = Path::new("/tmp/some-proj");
        let p = build_main_system_prompt(cwd, Some(proj));
        assert!(p.contains("项目根"));
    }

    #[test]
    fn prompt_contains_engineering_stance() {
        let p = build_main_system_prompt(Path::new("/tmp"), None);
        assert!(p.contains("工程立场"), "engineering stance section: {p}");
        assert!(p.contains("对终态负责"), "end-state responsibility: {p}");
        assert!(p.contains("根因"), "root-cause discipline: {p}");
    }

    #[test]
    fn prompt_contains_no_fabrication() {
        let p = build_main_system_prompt(Path::new("/tmp"), None);
        assert!(p.contains("不要编造"), "no-fabrication discipline: {p}");
    }

    #[test]
    fn prompt_contains_task_completion() {
        let p = build_main_system_prompt(Path::new("/tmp"), None);
        assert!(p.contains("完全解决"), "task completion discipline: {p}");
    }

    #[test]
    fn prompt_contains_validation_discipline() {
        let p = build_main_system_prompt(Path::new("/tmp"), None);
        assert!(p.contains("没跑过不要说过了"), "validation discipline: {p}");
    }

    #[test]
    fn prompt_contains_sandbox_boundary() {
        let p = build_main_system_prompt(Path::new("/tmp"), None);
        assert!(p.contains("工具沙箱边界"), "sandbox boundary section: {p}");
        assert!(p.contains("`.git` 目录只读"), ".git protected: {p}");
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
        let p = build_main_system_prompt(Path::new("/tmp"), None);
        assert!(
            !p.contains("当前 thread id"),
            "no thread id row in runtime identity block: {p}"
        );
    }

    #[test]
    fn static_prompt_is_embedded_verbatim() {
        // Editing the markdown must show through without rebuilding logic.
        let p = build_main_system_prompt(Path::new("/tmp"), None);
        assert!(p.contains("工程立场"));
        assert!(p.contains("进程内 native agent 工作台"));
    }

    #[test]
    fn max_turns_summary_prompt_contains_cap_and_no_tools() {
        let s = max_turns_summary_prompt(10);
        assert!(s.contains("10"), "cap value must appear: {s}");
        assert!(s.contains("不要再调用任何工具"), "no-tools directive: {s}");
    }
}
