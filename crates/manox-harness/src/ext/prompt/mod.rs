//! Prompt assembly for harness sessions — the extension-layer renderer
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
const SUBAGENTS_PROSE: &str = "You can delegate work to subagents through the single `Steer` \
    tool. A subagent is a fresh-context worker running a named definition (e.g. `Explore`, \
    `Sailor`, or a user-defined kind): it cannot see this conversation, so its `prompt` must \
    carry every fact, path, and done-criterion the work needs. A subagent cannot ask you or \
    the user questions mid-run, and its approval-requiring operations are auto-rejected — \
    surface those steps in your own work instead.\n\n\
    Dispatch with `reason: \"Dispatch\"`, `to.spawn` set to a definition name, and `prompt` \
    set to the task. Pick `to.spawn` values from the `ListAgents` tool: it lists every \
    available definition (name, description, capability, model) alongside your live runs — \
    never guess a definition name. A background definition (one whose entry says it always \
    runs in the background) ignores `run_in_background: false`.\n\n\
    By default a dispatch is foreground: it blocks until the child finishes, and the tool \
    result carries the child's final report (failures carry a diagnostic plus whatever \
    partial output existed). Set `run_in_background: true` to park the run on a background \
    card instead and receive the report as a peer message when the run settles — a completed \
    report, a timeout/failure report with partial output, or nothing for a run you \
    interrupted.\n\n\
    Every dispatch result carries the run's id — the run's permanent address. Guide a live \
    run mid-flight with `reason: \"Inject\"` and `to.agent_address` set to the run id (the \
    guidance lands in the child's current turn and counts as activity for its idle budget; \
    the ack confirms delivery, it is not the child's answer). Stop a run with `reason: \
    \"Abort\"`. Reopen a settled run by dispatching again with `to.agent_address` set to the \
    run id, no `to.spawn`, and the follow-up as `prompt` — the child keeps its full context. \
    Injecting into a run that already settled is an error: dispatch to the run id instead.\n\n\
    Prefer parallel subagents over serial self-work: for splittable tasks — reviewing multiple \
    PRs, modifying independent files, exploring alternatives — emit several background \
    dispatches in one turn so they run concurrently. Pass `isolation: \"worktree\"` when a \
    subagent needs its own working tree (builds won't collide, edits won't clash).\n\n\
    Time-box every dispatch: size `timeout_ms` to your estimate plus headroom — an expired \
    budget terminates the child and delivers a report you can act on, instead of a task that \
    runs forever. While runs are in flight, `ListAgents` reports each run's health \
    (working, tool running, stalled, looping) with its running time and counters — \
    inspect before acting, do not guess.\n\n\
    Delegation is not fire-and-forget; you own each subagent's lifecycle. When a report \
    arrives, decide deliberately: continue the run with a narrower instruction, widen the \
    budget, or take the work over yourself. To create a real manox Thread that persists, \
    appears in the sidebar, and can be resumed, use `Steer` with `to.spawn: \"TeamMember\"` \
    (only the Captain may spawn members).";

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
    /// The session's rendered user-notes section (the annotations channel's
    /// unresolved entries), maintained by the engine next to its journal
    /// seam and read at every render — a note written mid-turn reaches the
    /// very next request. `None` for sessions with no notes source (tests,
    /// subagents).
    pub user_notes: Option<std::sync::Arc<std::sync::RwLock<String>>>,
}

#[derive(Serialize)]
struct CaptainData {
    cwd: String,
    today: String,
    subagents_prose: &'static str,
    skills: Vec<SkillSummary>,
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
        },
    );
    let cwd = config.cwd;
    let notes = config.user_notes;
    std::sync::Arc::new(
        move |active_tools: &[String],
              resources: &crate::core::harness::HarnessResources,
              _session_cwd: &std::path::Path| {
            let mut assembly = render_assembly(&base, &cwd, active_tools, resources);
            // The notes section is a read-time fact: the cell holds the
            // journal's current annotations, so every render sees the notes
            // the latest durable state carries.
            if let Some(cell) = &notes
                && let Ok(section) = cell.read()
                && !section.is_empty()
            {
                assembly.push_str("\n\n# User notes\n\n");
                assembly.push_str(&section);
            }
            assembly
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
        move |active_tools: &[String],
              resources: &crate::core::harness::HarnessResources,
              _session_cwd: &std::path::Path| {
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
        user_notes: None,
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
    let cwd = std::path::Path::new("/private/tmp/golden-proj");
    builder(&active_tools, &resources, cwd)
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
            builder(
                &active_tools,
                &resources,
                std::path::Path::new("/private/tmp/golden-proj")
            ),
            include_str!("../../../testdata/fold_prompt.golden.txt"),
        );
    }
}
