//! Main-thread system prompt.
//!
//! Sub-agents carry their own `system` field loaded from `agents/*.md`; the
//! main thread has none (`Thread::system == None`), so this is minted fresh on
//! every request build — date changes, project may change — and prepended as a
//! `System` message by `Thread::build_completion_request`. The prompt injects
//! the runtime identity (thread id, cwd, project, os, shell, date) the model
//! could not otherwise obtain without querying external state, plus working
//! discipline that the harness cannot enforce structurally.

use std::path::Path;

/// Build the main-thread system prompt from live thread state.
///
/// Sub-agents never call this — their `system` field is `Some`, so
/// `build_completion_request` takes the `unwrap_or_else` branch only for the
/// main thread. Keep the prompt stable in shape so the Anthropic wire mapper
/// can cache it (`cache: true` on the System message).
pub fn build_main_system_prompt(cwd: &Path, project: Option<&Path>, thread_id: &str) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let os = std::env::consts::OS;

    let mut s = String::new();
    s.push_str("你是 manox agent，一个基于 GPUI 的进程内 native agent 工作台。\n\n");

    s.push_str("## 运行时身份\n");
    s.push_str(&format!("- 当前 thread id：`{thread_id}`\n"));
    s.push_str(&format!("- 当前工作目录：`{}`\n", cwd.display()));
    if let Some(p) = project {
        s.push_str(&format!("- 项目根：`{}`\n", p.display()));
    }
    s.push_str(&format!("- 操作系统：{os}\n"));
    s.push_str(&format!("- 默认 shell：{shell}\n"));
    s.push_str(&format!("- 今天：{today}\n\n"));

    s.push_str("## 工作纪律\n");
    s.push_str("- 所有相对路径相对于「当前工作目录」解析。\n");
    s.push_str(
        "- 不要用 `cd` 切换到其他 git worktree 或当前工作目录之外的路径；\
          如需在别处操作，用绝对路径并说明理由。\n",
    );
    s.push_str(
        "- 工具输出若被截断（出现 `⚠` 截断标注），用更窄的命令重试\
          （如指定列、`| head`、`LIMIT`），不要臆测被截断的内容。\n",
    );
    s.push_str(
        "- 执行 `git commit` 前，先跑 `git diff --cached` 核实将要提交的改动；\
          若 `nothing to commit`，说明你没有实际改动文件，不要谎报成功。\n",
    );

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
}
