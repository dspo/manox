//! Main-thread system prompt.
//!
//! Sub-agents carry their own `system` field loaded from `agents/*.md`; the
//! main thread has none (`Thread::system == None`), so this is minted fresh on
//! every request build — date changes, project may change — and prepended as a
//! `System` message by `Thread::build_completion_request`.
//!
//! This module probes the live environment (date, shell, python/node versions,
//! approval mode, active worktree, advertised skills, reply language) and packs
//! it into a [`crate::prompt::MainSystemPromptData`]. The layout — section
//! order, headings, list rows — lives in the `system/main.tera.md` template;
//! nothing here formats model-visible prose. The static body is embedded via
//! `include_str!` from [`system_prompt.md`] and carried as a `&'static str`
//! data field so prose edits never touch Rust.
//!
//! **Static-first layering for prefix-cache stability.** The template emits
//! most-static → most-volatile so the provider's prefix cache hits the longest
//! possible byte run turn-over-turn: (1) the compile-time static prose, (2) the
//! skills block (session-stable), (3) the language directive (locale-stable),
//! (4) the runtime identity block. Within the identity block, session-stable
//! rows (cwd/project/os/shell) come before daily-volatile `today` and
//! toggle-volatile approval mode. Thread id is deliberately NOT injected — the
//! model fetches it on demand via the `self_info` tool.
//!
//! The prompt prose language follows the thread's immutable agent axis
//! ([`crate::language::Language`]); this module renders the main-thread head,
//! while sub-agents carry their own `system` field and get the same directive
//! appended by the `system/assembly` template. The user's preferred reply
//! language is conveyed by a one-line directive baked into the main template
//! via [`language_data`].

use std::path::Path;
use std::sync::OnceLock;

use crate::language::Language;
use crate::prompt::{LanguagePromptData, MainSystemPromptData, PromptTemplate, render};
use crate::thread::ApprovalMode;

const STATIC_PROMPT_EN: &str = include_str!("system_prompt.en.md");
const STATIC_PROMPT_ZH_CN: &str = include_str!("system_prompt.zh-CN.md");

/// The static prose half of the main system prompt, in `lang`. The volatile
/// identity block lands at the very end (see [`build_main_system_prompt`]), so
/// the language-specific prose is a byte-stable prefix per agent language.
fn static_prompt(lang: Language) -> &'static str {
    match lang {
        Language::En => STATIC_PROMPT_EN,
        Language::ZhCn => STATIC_PROMPT_ZH_CN,
    }
}

/// Build the main-thread system prompt from live thread state.
///
/// Sub-agents never call this — their `system` field is `Some`, so
/// `build_completion_request` takes the `unwrap_or_else` branch only for the
/// main thread.
///
/// `agent_language` is the thread's immutable agent axis (not the process-global
/// UI locale) so two threads in different agent languages render distinct prose
/// concurrently; it selects the reply-language directive baked into the main
/// template.
///
/// Assembly order is static-first (see module docs): the volatile identity
/// block lands at the very end so toggling `approval_mode` or a day rollover
/// only invalidates the cached tail, not the static prose. The plan-mode
/// directive is injected as a user message by `set_plan_mode` (not here), so
/// toggling plan mode likewise only busts the tail.
pub fn build_main_system_prompt(
    cwd: &Path,
    project: Option<&Path>,
    approval_mode: ApprovalMode,
    active_worktree: Option<(&str, &Path)>,
    agent_language: Language,
) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let os = std::env::consts::OS;
    let (python3, node) = runtime_versions();

    // `None` approval mode stays silent (the default `OnRequest` case), keeping
    // the identity block byte-stable for the common path. AutoReview and Yolo
    // are the two modes the model can act differently on, so only those are
    // advertised — without revealing the internal reviewer mechanism.
    let approval_mode = match approval_mode {
        ApprovalMode::OnRequest => None,
        ApprovalMode::AutoReview => Some("AutoReview"),
        ApprovalMode::Yolo => Some("Yolo"),
    };

    let data = MainSystemPromptData {
        static_body: static_prompt(agent_language),
        skills: crate::skill::summaries_or_empty(),
        language: language_data(agent_language),
        runtime: crate::prompt::RuntimeIdentityPromptData {
            cwd: cwd.display().to_string(),
            project: project.map(|p| p.display().to_string()),
            active_worktree: active_worktree.map(|(branch, path)| {
                crate::prompt::WorktreePromptData {
                    branch: branch.to_string(),
                    path: path.display().to_string(),
                }
            }),
            os,
            shell,
            python3: python3.to_string(),
            node: node.to_string(),
            today,
            approval_mode,
        },
    };
    render(PromptTemplate::SystemMain, agent_language, &data).expect("system main template render")
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

/// The language directive baked into the system prompt so the model addresses
/// the user in the thread's agent language. The directive is selected by the
/// thread's immutable [`Language`] (not the process-global UI locale), so the
/// render API stays explicit about which language a given conversation is in
/// and two threads in different agent languages can run concurrently. Returned
/// as a data payload; the `system/main` and `system/assembly` templates own
/// the surrounding `## Language` layout.
pub fn language_data(agent_language: Language) -> LanguagePromptData {
    // The name is always an English endonym ("English", "Simplified Chinese") —
    // the model parses the directive, the user never sees this string.
    LanguagePromptData {
        language: agent_language.english_name(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_cwd_and_identity() {
        let cwd = Path::new("/tmp/some-proj");
        let p = build_main_system_prompt(cwd, None, ApprovalMode::OnRequest, None, Language::En);
        assert!(p.contains("/tmp/some-proj"), "cwd must appear: {p}");
        assert!(p.contains("You are manox"), "identity must appear: {p}");
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
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
        assert!(!p.contains("GPUI"), "must not leak tech stack: {p}");
        assert!(!p.contains("gpui"), "must not leak tech stack: {p}");
        assert!(!p.contains("brush"), "must not leak tech stack: {p}");
    }

    #[test]
    fn runtime_identity_block_appended_at_tail() {
        // Identity lands at the very end of the prompt — after the static
        // prose, skills, and language directive — so the cacheable static
        // prefix is maximal.
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
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
    fn prompt_contains_manox_specific_tool_contracts() {
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
        assert!(p.contains("Tool contracts"), "tool contracts section: {p}");
        assert!(p.contains("don't re-Read"), "edit/read contract: {p}");
        assert!(p.contains('⚠'), "truncation contract: {p}");
    }

    #[test]
    fn prompt_includes_project_when_set() {
        let cwd = Path::new("/tmp/some-proj");
        let proj = Path::new("/tmp/some-proj");
        let p =
            build_main_system_prompt(cwd, Some(proj), ApprovalMode::OnRequest, None, Language::En);
        assert!(p.contains("Project root"));
    }

    #[test]
    fn prompt_omits_generic_model_tutoring() {
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
        assert!(
            !p.contains("Context economy"),
            "generic context coaching must stay out of the static prompt: {p}"
        );
        assert!(
            !p.contains("Pass arguments strictly"),
            "generic tool coaching must stay out of the static prompt: {p}"
        );
        assert!(
            !p.contains("independent tool calls in parallel"),
            "generic parallelism coaching must stay out of the static prompt: {p}"
        );
    }

    // The "don't fabricate" / "no fabrication" discipline was carried by the
    // "Engineering stance" section of the old (verbose) system prompt. The
    // trimmed prompt removes generic best-practice prose that strong models
    // already know; the guard against fabrication is now implicit in the
    // overall instruction tone + tool-sandbox boundary. No dedicated test
    // needed — the prompt's functional correctness is covered by the
    // verification-discipline and static-prompt-embedded tests.

    #[test]
    fn prompt_contains_sandbox_boundary() {
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
        assert!(
            p.contains("Tool sandbox boundary"),
            "sandbox boundary section: {p}"
        );
        assert!(p.contains("`.git` is read-only"), ".git protected: {p}");
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
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
        assert!(
            !p.contains("Current thread id"),
            "no thread id row in runtime identity block: {p}"
        );
    }

    #[test]
    fn static_prompt_is_embedded_verbatim() {
        // Editing the markdown must show through without rebuilding logic.
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
        assert!(p.contains("Tool contracts"));
        assert!(p.contains("in-process native coding agent"));
    }

    #[test]
    fn prompt_injects_language_directive() {
        // The current-locale language directive must land in the built prompt.
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
        assert!(p.contains("## Language"), "language section missing: {p}");
        assert!(
            p.contains("write your user-facing responses in"),
            "language directive missing: {p}"
        );
    }

    #[test]
    fn yolo_mode_advertised_when_enabled() {
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::Yolo,
            None,
            Language::En,
        );
        assert!(p.contains("YOLO"), "yolo mode line missing: {p}");
    }

    #[test]
    fn yolo_mode_silent_when_disabled() {
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
        assert!(
            !p.contains("YOLO"),
            "yolo must not appear when disabled: {p}"
        );
    }

    #[test]
    fn zh_cn_prompt_renders_chinese_prose_and_directive() {
        // The zh-CN static body, identity block, and language directive must
        // all land in Chinese when the thread's agent language is ZhCn. Guards
        // against `system_prompt.zh-CN.md` prose regressions and a miswired
        // `static_prompt(lang)` selector that silently falls back to English.
        let p = build_main_system_prompt(
            Path::new("/tmp/some-proj"),
            None,
            ApprovalMode::Yolo,
            None,
            Language::ZhCn,
        );
        assert!(p.contains("工具契约"), "zh-CN static prose missing: {p}");
        assert!(p.contains("## 运行时身份"), "zh-CN identity heading: {p}");
        assert!(p.contains("今天："), "zh-CN today row: {p}");
        assert!(p.contains("/tmp/some-proj"), "cwd must appear: {p}");
        assert!(
            p.contains("用Simplified Chinese撰写"),
            "zh-CN language directive missing: {p}"
        );
        assert!(
            p.contains("YOLO"),
            "yolo must still be advertised in zh-CN: {p}"
        );
        assert!(
            !p.contains("## Runtime identity"),
            "english identity heading must not leak into zh-CN: {p}"
        );
    }

    #[test]
    fn prompt_contains_multi_agent_delegation() {
        // The trimmed prompt condenses the old verbose sections into concise
        // tool-usage rules. "Multi-agent delegation" carries the key contract:
        // Agent, TeamCreate, and write-safety across members.
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
        assert!(p.contains("Multi-agent work"), "multi-agent section: {p}");
        assert!(p.contains("`Agent`"), "agent tool mention: {p}");
        assert!(p.contains("TeamCreate"), "team-create mention: {p}");
    }

    #[test]
    fn prompt_contains_git_verification_discipline() {
        // Every git write op must be verified with git itself before reporting
        // success — the branch-tracking false-success regression (thread
        // e5047fd2) came from reporting push success without checking
        // `git log origin/<branch>`.
        let p = build_main_system_prompt(
            Path::new("/tmp"),
            None,
            ApprovalMode::OnRequest,
            None,
            Language::En,
        );
        assert!(p.contains("Git operations"), "git section: {p}");
        assert!(
            p.contains("git log origin/<branch>"),
            "remote verification: {p}"
        );
        assert!(
            p.contains("After pushing, verify"),
            "push verification: {p}"
        );
        assert!(
            p.contains("git branch --show-current"),
            "branch name from measurement: {p}"
        );
    }
}
