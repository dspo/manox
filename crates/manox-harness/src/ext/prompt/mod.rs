//! Prompt assembly for pi harness sessions — the extension-layer renderer
//! that owns the system prompt.
//!
//! Templates live under `templates/<preset>/`; this module registers the
//! `default` preset's template set. A future preset system adds sibling
//! preset directories plus a selection API without changing the render seam:
//! callers always receive a [`crate::core::harness::SystemPromptBuilder`].
//!
//! Byte fidelity is load-bearing: the system prompt leads the provider
//! request's cached prefix, so the rendered output must stay byte-identical
//! to the assembly it replaces. Golden files under `testdata/` pin the bytes.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Serialize;

const CAPTAIN_KEY: &str = "default/captain.tera.md";
const ASSEMBLY_KEY: &str = "default/assembly.tera.md";

const TPL_CAPTAIN: &str = include_str!("../../../templates/default/captain.tera.md");
const TPL_ASSEMBLY: &str = include_str!("../../../templates/default/assembly.tera.md");

/// The Captain's subagent-dispatch prose. Prose is data — the template owns
/// layout only — so prose edits never touch the render path.
const SUBAGENTS_PROSE: &str = "You can dispatch subagents via the `Steer` tool — each runs in its \
    own fresh context (no parent history). Set `to.spawn` to a capability \
    def name (e.g. 'Sailor','Explore') to create an in-thread subagent \
    coroutine, or 'TeamMember' to create a real manox Thread (a process: \
    persisted, sidebar-visible, resumable, with its own Captain session). \
    Only the Captain may spawn. `reason` is Dispatch (start a task), \
    Inject (mid-run message), or Abort (cancel). Complete is harness-\
    emitted on subagent termination — not callable; when a subagent \
    finishes, its final summary arrives as a peer message and triggers \
    your next turn. Continue with other work while subagents run; do \
    not poll.\n\n\
    Built-in subagent types: `Explore` (read-only: locates code by \
    file, symbol, or keyword) and `Sailor` (write+bash: reads, writes, \
    edits files, runs shell commands including cargo/clippy/test). \
    TeamMembers are real threads — they persist, appear in the sidebar, \
    and can be resumed; members do not report back autonomously — observe \
    their progress by opening their tab and Inject guidance as needed \
    (they cannot reply through the bus yet). A subagent (coroutine) is \
    transient; it ends with a concise summary as its final assistant \
    text (the harness emits Complete with that summary).\n\n\
    Prefer parallel subagents over serial self-work. For splittable \
    tasks — reviewing multiple PRs, modifying independent files, \
    exploring alternatives — emit multiple `Steer` calls in one turn \
    so they run concurrently. Pass `isolation: \"worktree\"` when a \
    subagent needs its own working tree (builds won't collide, edits \
    won't clash).\n\n\
    Each subagent starts from a blank context, so pin any contract it \
    must honor (exact paths, signatures, gate requirements) directly in \
    the prompt.\n\n\
    Time-box every dispatch: estimate how long each subtask should take \
    and split work that won't fit one bounded slice into several bounded \
    dispatches instead of a single open-ended giant. Give each dispatch a \
    `timeout` sized to your estimate plus headroom — an expired budget \
    terminates the subagent and delivers a timeout report you can act on, \
    instead of a task that runs forever.\n\n\
    Make every dispatch concrete and checkable: an explicit done-criterion \
    (which question answered, which gate passed) and the shape of the \
    summary you expect back.\n\n\
    Dispatch is not fire-and-forget; you own each subagent's lifecycle. \
    Watch for its Complete, timeout, or failure report, and when one \
    arrives decide deliberately: re-dispatch with a narrower scope, widen \
    the budget, or take the work over yourself. Abort a subagent the \
    moment its work is no longer needed.\n\n\
    When a subagent seems slow or silent, inspect it before acting: \
    `SubagentStatus` reports each live subagent's health (working, tool \
    running, stalled, looping) with its turns, tool calls, running time, \
    and remaining budget. Act on what it reports — Inject guidance into a \
    drifting run, Abort a stalled or looping one and re-dispatch narrower \
    — instead of guessing or waiting indefinitely.";

/// The process-global registry of built-in prompt templates. Parsed once;
/// immutable thereafter. A parse failure panics at first use — these are
/// compile-time-embedded assets, so a parse error is a build defect.
fn tera() -> &'static tera::Tera {
    static TERA: OnceLock<tera::Tera> = OnceLock::new();
    TERA.get_or_init(|| {
        let mut tera = tera::Tera::default();
        for (key, src) in [(CAPTAIN_KEY, TPL_CAPTAIN), (ASSEMBLY_KEY, TPL_ASSEMBLY)] {
            tera.add_raw_template(key, src)
                .unwrap_or_else(|e| panic!("built-in prompt template {key} failed to parse: {e}"));
        }
        tera
    })
}

/// Render a registered template. The builder signature returns a plain
/// `String` (no `Result` channel), and both the templates and their payloads
/// are built-in, so a render failure is a bug and panics with the template
/// name rather than surfacing a half-assembled prompt.
fn render(key: &str, data: &impl Serialize) -> String {
    let ctx = tera::Context::from_serialize(data).unwrap_or_else(|e| {
        panic!("built-in prompt template {key} payload failed to serialize: {e}")
    });
    tera()
        .render(key, &ctx)
        .unwrap_or_else(|e| panic!("built-in prompt template {key} failed to render: {e}"))
}

/// One-line skill summary advertised in the Captain system prompt.
#[derive(Debug, Clone, Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

/// Session-stable inputs of the Captain system prompt, captured once at
/// session creation. The returned builder re-renders only the fold (project
/// context, cwd trailer) on each active-tool/resource rebuild.
pub struct CaptainConfig {
    pub cwd: PathBuf,
    pub today: String,
    pub skills: Vec<SkillSummary>,
    /// Comma-separated LSP server spec ids that are ready (e.g.
    /// "rust-analyzer, gopls"). Empty when no LSP servers are available.
    /// Injected as a dynamic line in the system prompt so the model knows
    /// which languages have code intelligence without needing explicit
    /// LspEnsure/LspWaitReady calls.
    pub lsp_ready_specs: String,
}

#[derive(Serialize)]
struct CaptainData {
    cwd: String,
    today: String,
    subagents_prose: &'static str,
    skills: Vec<SkillSummary>,
    lsp_ready_specs: String,
}

/// A project-instruction file in the fold. `location` is XML-escaped,
/// `content` is inserted raw — byte-parity with the kernel fold.
#[derive(Serialize)]
struct CtxFileData {
    location: String,
    content: String,
}

/// A harness skill advertised by the fold; every field XML-escaped.
#[derive(Serialize)]
struct AdvSkillData {
    name: String,
    description: String,
    location: String,
}

#[derive(Serialize)]
struct AssemblyData<'a> {
    base: &'a str,
    cwd: String,
    context_files: Vec<CtxFileData>,
    /// Kernel parity: harness skills are advertised only when the `Read`
    /// tool is active.
    skills_advertised: bool,
    skills: Vec<AdvSkillData>,
}

fn assembly_data<'a>(
    base: &'a str,
    cwd: &std::path::Path,
    active_tools: &[String],
    resources: &crate::core::harness::HarnessResources,
) -> AssemblyData<'a> {
    AssemblyData {
        base,
        cwd: cwd.display().to_string(),
        context_files: resources
            .context_files
            .iter()
            .map(|f| CtxFileData {
                location: crate::core::system_prompt::xml_escape(&f.location),
                content: f.content.clone(),
            })
            .collect(),
        skills_advertised: active_tools.iter().any(|t| t == "Read") && !resources.skills.is_empty(),
        skills: resources
            .skills
            .iter()
            .map(|s| AdvSkillData {
                name: crate::core::system_prompt::xml_escape(&s.name),
                description: crate::core::system_prompt::xml_escape(&s.description),
                location: crate::core::system_prompt::xml_escape(&s.location),
            })
            .collect(),
    }
}

fn render_assembly(
    base: &str,
    cwd: &std::path::Path,
    active_tools: &[String],
    resources: &crate::core::harness::HarnessResources,
) -> String {
    render(
        ASSEMBLY_KEY,
        &assembly_data(base, cwd, active_tools, resources),
    )
}

/// The Captain session's [`crate::core::harness::SystemPromptBuilder`]: the Captain
/// base is rendered once from the config; the fold re-renders on every
/// active-tool/resource rebuild.
pub fn captain_prompt_builder(config: CaptainConfig) -> crate::core::harness::SystemPromptBuilder {
    let base = render(
        CAPTAIN_KEY,
        &CaptainData {
            cwd: config.cwd.display().to_string(),
            today: config.today,
            subagents_prose: SUBAGENTS_PROSE,
            skills: config.skills,
            lsp_ready_specs: config.lsp_ready_specs,
        },
    );
    let cwd = config.cwd;
    std::sync::Arc::new(
        move |active_tools: &[String], resources: &crate::core::harness::HarnessResources| {
            render_assembly(&base, &cwd, active_tools, resources)
        },
    )
}

/// A [`crate::core::harness::SystemPromptBuilder`] for sessions that bring their own
/// base prose (subagent `agents/*.md` bodies): the same fold the kernel
/// applies to a custom prompt, rendered from the assembly template.
pub fn base_prompt_builder(
    base: String,
    cwd: PathBuf,
) -> crate::core::harness::SystemPromptBuilder {
    std::sync::Arc::new(
        move |active_tools: &[String], resources: &crate::core::harness::HarnessResources| {
            render_assembly(&base, &cwd, active_tools, resources)
        },
    )
}

/// The golden fixture render — the single source of the byte-pinning
/// test's input AND of `examples/dump_prompt_golden.rs`, so regenerating
/// `testdata/captain_prompt.golden.txt` can never drift from what the
/// test asserts.
#[doc(hidden)]
pub fn render_golden_fixture() -> String {
    let builder = captain_prompt_builder(CaptainConfig {
        cwd: PathBuf::from("/private/tmp/golden-proj"),
        today: "2026-08-25".to_string(),
        skills: vec![
            SkillSummary {
                name: "gitwork:deliver".into(),
                description: "deliver a PR".into(),
            },
            SkillSummary {
                name: "remora:task".into(),
                description: "delegate a stuck problem".into(),
            },
        ],
        lsp_ready_specs: String::new(),
    });
    let resources = crate::core::harness::HarnessResources {
        skills: vec![],
        prompt_templates: vec![],
        context_files: vec![
            crate::core::harness::ContextFile {
                name: "CLAUDE.md".into(),
                location: "/private/tmp/golden-proj/CLAUDE.md".into(),
                content: "Keep changes minimal.\nLine two.".into(),
            },
            crate::core::harness::ContextFile {
                name: "RULES.md".into(),
                location: "/tmp/a&b<R>/rules.md".into(),
                content: "r1".into(),
            },
        ],
    };
    let active_tools = vec![
        "Read".to_string(),
        "Bash".to_string(),
        "Edit".to_string(),
        "Write".to_string(),
    ];
    builder(&active_tools, &resources)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The golden bytes pin the rendered system-prompt bytes — the leading
    /// cached prefix of every provider request — against template drift;
    /// regenerate via `examples/dump_prompt_golden.rs` (same fixture by
    /// construction).
    #[test]
    fn captain_prompt_matches_golden_bytes() {
        assert_eq!(
            render_golden_fixture(),
            include_str!("../../../testdata/captain_prompt.golden.txt"),
        );
    }

    #[test]
    fn subagent_fold_matches_golden_bytes() {
        let builder = base_prompt_builder(
            "You are the Explore agent.\n\nSearch carefully.".to_string(),
            PathBuf::from("/private/tmp/golden-proj"),
        );
        let resources = crate::core::harness::HarnessResources::default();
        let active_tools = vec!["Read".into(), "Grep".into(), "Glob".into(), "Ls".into()];
        assert_eq!(
            builder(&active_tools, &resources),
            include_str!("../../../testdata/fold_prompt.golden.txt"),
        );
    }
}
