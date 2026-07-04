//! Main-thread system prompt.
//!
//! Sub-agents carry their own `system` field loaded from `agents/*.md`; the
//! main thread has none (`Thread::system == None`), so this is minted fresh on
//! every request build — date changes, project may change — and prepended as a
//! `System` message by `Thread::build_completion_request`.
//!
//! The static body (identity + working discipline) lives in
//! [`system_prompt.md`] next to this file, embedded via `include_str!` so the
//! prose reads as plain markdown and edits don't touch Rust — mirroring codex's
//! split (static `base_instructions/default.md` + dynamic `<cwd>`/`
//! <current_date>` environment XML). Only the runtime identity block (thread
//! id, cwd, project, os, shell, date) is formatted here, since those are
//! machine-assembled key/value rows, not prose.

use std::path::Path;

const STATIC_PROMPT: &str = include_str!("system_prompt.md");

/// Build the main-thread system prompt from live thread state.
///
/// Sub-agents never call this — their `system` field is `Some`, so
/// `build_completion_request` takes the `unwrap_or_else` branch only for the
/// main thread.
pub fn build_main_system_prompt(cwd: &Path, project: Option<&Path>, thread_id: &str) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let os = std::env::consts::OS;

    let mut s = STATIC_PROMPT.to_string();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("\n## 运行时身份\n");
    s.push_str(&format!("- 当前 thread id：`{thread_id}`\n"));
    s.push_str(&format!("- 当前工作目录：`{}`\n", cwd.display()));
    if let Some(p) = project {
        s.push_str(&format!("- 项目根：`{}`\n", p.display()));
    }
    s.push_str(&format!("- 操作系统：{os}\n"));
    s.push_str(&format!("- 默认 shell：{shell}\n"));
    s.push_str(&format!("- 今天：{today}\n"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_thread_id_cwd_and_identity() {
        let cwd = Path::new("/tmp/some-proj");
        let p = build_main_system_prompt(cwd, None, "deadbeef-1234");
        assert!(p.contains("deadbeef-1234"), "thread id must appear: {p}");
        assert!(p.contains("/tmp/some-proj"), "cwd must appear: {p}");
        assert!(p.contains("manox agent"), "identity must appear: {p}");
        assert!(p.contains("今天"), "date must appear: {p}");
    }

    #[test]
    fn prompt_does_not_leak_tech_stack() {
        // The identity names the product, not the implementation: the model has
        // no use for "GPUI" or other framework names, and exposing them only
        // invites tangents.
        let p = build_main_system_prompt(Path::new("/tmp"), None, "x");
        assert!(!p.contains("GPUI"), "must not leak tech stack: {p}");
        assert!(!p.contains("gpui"), "must not leak tech stack: {p}");
    }

    #[test]
    fn prompt_includes_project_when_set() {
        let cwd = Path::new("/tmp/some-proj");
        let proj = Path::new("/tmp/some-proj");
        let p = build_main_system_prompt(cwd, Some(proj), "abc");
        assert!(p.contains("项目根"));
    }

    #[test]
    fn prompt_includes_truncation_and_commit_discipline() {
        let p = build_main_system_prompt(Path::new("/tmp"), None, "x");
        assert!(p.contains("截断"), "truncation discipline: {p}");
        assert!(p.contains("git diff --cached"), "commit discipline: {p}");
        assert!(p.contains("cd"), "cwd discipline: {p}");
    }

    #[test]
    fn static_prompt_is_embedded_verbatim() {
        // Editing the markdown must show through without rebuilding logic.
        let p = build_main_system_prompt(Path::new("/tmp"), None, "x");
        assert!(p.contains("工作纪律"));
        assert!(p.contains("进程内 native agent 工作台"));
    }
}
