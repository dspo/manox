//! The single Tera instance + the only place outside tests that touches
//! `tera::`.
//!
//! Every model-visible prompt is rendered here. The module boundary rule:
//! callers pass a [`PromptTemplate`] key + a [`Serialize`] payload and get a
//! `String` back — they never construct a `tera::Tera` / `tera::Context`
//! themselves. One process-global [`Tera`] (lazily initialized via
//! [`OnceLock`]) registers every built-in `.tera.md` embedded at compile time,
//! so the parse cost is paid once and the registry is immutable thereafter.
//!
//! Model-facing prose is English only: this repository ships a single copy of
//! every prompt template and never selects between locales. User-facing chrome
//! localization belongs entirely to the host application and never flows here.
//!
//! Autoescape is off: these are model-facing markdown/text prompts, not HTML,
//! so `{{ var }}` inserts raw bytes. Variables are inserted as opaque text
//! (never re-parsed as template syntax), so a `static_body` carrying literal
//! `{{` is safe.

use std::sync::OnceLock;

use serde::Serialize;

use crate::language_model::{
    LanguageModelRequestMessage, LanguageModelRequestTool, MessageContent, Role,
};
use crate::prompt::template::{self, PromptTemplate};

// Compile-time-embedded template sources — the single English copy of every
// built-in prompt. Each const mirrors the [`PromptTemplate::name`] registry key,
// so a missing file fails the build rather than surfacing as a runtime render
// error.
const TPL_SYSTEM_MAIN: &str = include_str!("templates/system/main.tera.md");
const TPL_SYSTEM_ASSEMBLY: &str = include_str!("templates/system/assembly.tera.md");
const TPL_WRAPPER_MAX_TURNS_SUMMARY: &str =
    include_str!("templates/wrapper/max_turns_summary.tera.md");
const TPL_WRAPPER_MAX_TOKENS_DIRECTIVE: &str =
    include_str!("templates/wrapper/max_tokens_directive.tera.md");
const TPL_WRAPPER_RECOVERY_FAILURE: &str =
    include_str!("templates/wrapper/recovery_failure.tera.md");
const TPL_WRAPPER_EMPTY_TURN_NUDGE: &str =
    include_str!("templates/wrapper/empty_turn_nudge.tera.md");
const TPL_WRAPPER_UNFULFILLED_TOOL_INTENT_NUDGE: &str =
    include_str!("templates/wrapper/unfulfilled_tool_intent_nudge.tera.md");
const TPL_WRAPPER_DENIAL_BREAKER_DIRECTIVE: &str =
    include_str!("templates/wrapper/denial_breaker_directive.tera.md");
const TPL_WRAPPER_PEER_MESSAGE: &str = include_str!("templates/wrapper/peer_message.tera.md");
const TPL_WRAPPER_TOOL_DENIED: &str = include_str!("templates/wrapper/tool_denied.tera.md");
const TPL_TITLE_FIRST: &str = include_str!("templates/title/first.tera.md");
const TPL_TITLE_TOPIC_SHIFT: &str = include_str!("templates/title/topic_shift.tera.md");
const TPL_SKILL_BODY: &str = include_str!("templates/wrapper/skill_body.tera.md");
const TPL_PLAN_MODE_ACTIVE: &str = include_str!("templates/mode/plan_mode_active.tera.md");
const TPL_PLAN_MODE_APPROVED: &str = include_str!("templates/mode/plan_mode_approved.tera.md");

/// `(PromptTemplate, source)` for every built-in template: the single source of
/// truth for what gets parsed into the Tera registry.
const REGISTRATIONS: &[(PromptTemplate, &str)] = &[
    (PromptTemplate::SystemMain, TPL_SYSTEM_MAIN),
    (PromptTemplate::SystemAssembly, TPL_SYSTEM_ASSEMBLY),
    (
        PromptTemplate::WrapperMaxTurnsSummary,
        TPL_WRAPPER_MAX_TURNS_SUMMARY,
    ),
    (
        PromptTemplate::WrapperMaxTokensDirective,
        TPL_WRAPPER_MAX_TOKENS_DIRECTIVE,
    ),
    (
        PromptTemplate::WrapperRecoveryFailure,
        TPL_WRAPPER_RECOVERY_FAILURE,
    ),
    (
        PromptTemplate::WrapperEmptyTurnNudge,
        TPL_WRAPPER_EMPTY_TURN_NUDGE,
    ),
    (
        PromptTemplate::WrapperUnfulfilledToolIntentNudge,
        TPL_WRAPPER_UNFULFILLED_TOOL_INTENT_NUDGE,
    ),
    (
        PromptTemplate::WrapperDenialBreakerDirective,
        TPL_WRAPPER_DENIAL_BREAKER_DIRECTIVE,
    ),
    (PromptTemplate::WrapperPeerMessage, TPL_WRAPPER_PEER_MESSAGE),
    (PromptTemplate::WrapperToolDenied, TPL_WRAPPER_TOOL_DENIED),
    (PromptTemplate::TitleFirstInstruction, TPL_TITLE_FIRST),
    (
        PromptTemplate::TitleTopicShiftInstruction,
        TPL_TITLE_TOPIC_SHIFT,
    ),
    (PromptTemplate::SkillBody, TPL_SKILL_BODY),
    (PromptTemplate::ModePlanActive, TPL_PLAN_MODE_ACTIVE),
    (PromptTemplate::ModePlanApproved, TPL_PLAN_MODE_APPROVED),
];

/// The lazily-initialized global Tera registry. Holds every built-in template
/// parsed once; immutable for the process lifetime after first use.
fn tera() -> &'static tera::Tera {
    static TERA: OnceLock<tera::Tera> = OnceLock::new();
    TERA.get_or_init(build_tera)
}

/// Parse every registered template source into a fresh [`Tera`]. One
/// (variant, source) pair per built-in template. A parse failure panics at
/// first use (see [`assert_all_registered`]) rather than at a deferred render
/// site. Tera resolves `{% include %}` at `add_raw_template` time, so the
/// `mode/*` targets must be registered before `system/assembly.tera.md` (which
/// includes them) is parsed — the [`REGISTRATIONS`] order already guarantees
/// that.
fn build_tera() -> tera::Tera {
    let mut tera = tera::Tera::default();
    // Autoescape is off by default for non-HTML template extensions; the
    // `.tera.md` templates are model-facing markdown, so `{{ var }}`
    // inserts raw bytes. (Variables are never re-parsed as template
    // syntax, so a `static_body` carrying literal `{{` is safe.)
    for (variant, src) in REGISTRATIONS {
        tera.add_raw_template(variant.name(), src)
            .unwrap_or_else(|e| {
                panic!("built-in prompt template {variant:?} failed to parse: {e}")
            });
    }
    assert_all_registered(&tera);
    tera
}

/// Every [`PromptTemplate`] variant (per [`template::ALL`]) must have a
/// registered, parsed source in the Tera. Catches "added a variant, forgot the
/// `TPL_*` const or the registration row" at first use rather than at a
/// deferred render site.
fn assert_all_registered(tera: &tera::Tera) {
    // `ALL`, `REGISTRATIONS`, and the `TPL_*` set are all hand-maintained. A
    // variant added to one but not the others would slip past the per-variant
    // checks below — tie their lengths here so drift panics at first use
    // instead of silently leaving a variant unrenderable.
    assert_eq!(
        template::ALL.len(),
        REGISTRATIONS.len(),
        "template::ALL ({} entries) and REGISTRATIONS ({} rows) drifted \
         out of sync — a variant was added to one but not the other",
        template::ALL.len(),
        REGISTRATIONS.len(),
    );
    let parsed: std::collections::HashSet<&str> = tera.get_template_names().collect();
    for variant in template::ALL {
        let name = variant.name();
        assert!(
            parsed.contains(name),
            "PromptTemplate variant `{name}` is not parsed into the Tera \
             — its source const or registration row is missing"
        );
    }
}

/// Render `template` with `data`. The single materialize entry point: every
/// model-visible prompt string is produced here. Returns the rendered text;
/// errors surface as `anyhow::Error` so the boundary can `?`-propagate.
///
/// For templates with no variables, pass `&()`.
pub fn render<D: Serialize>(template: PromptTemplate, data: &D) -> anyhow::Result<String> {
    let tera = tera();
    let ctx = tera::Context::from_serialize(data)?;
    Ok(tera.render(template.name(), &ctx)?)
}

/// Render a no-variable template. Convenience for static prose (mode
/// addendums, side-call system prompts) that carries no payload.
pub fn render_static(template: PromptTemplate) -> anyhow::Result<String> {
    render(template, &std::collections::HashMap::<&str, &str>::new())
}

/// Render a slash-command body, substituting `arguments` into the
/// `{{ arguments }}` placeholder.
///
/// Command bodies are loaded from disk at runtime (user / plugin-authored),
/// so unlike the built-in compile-time templates they cannot be pre-registered
/// — they are rendered via Tera's one-off path against the live body string,
/// and intentionally not routed through the registry: a command body is
/// untrusted prose authored in whatever language its author chose, not a
/// manox-maintained asset. For backwards compatibility the legacy
/// `$ARGUMENTS` placeholder is rewritten to `{{ arguments }}` first, so old
/// command files keep working without a rewrite. If the body contains
/// Tera-incompatible literal syntax (an unmatched `{%` / `{{`, or an unknown
/// variable), the one-off render fails and the function falls back to a plain
/// string substitution — command bodies are untrusted prose, and a literal
/// `{{` must never break a command.
///
/// This is the single site that substitutes command arguments; no `replace`
/// of `$ARGUMENTS` lives in `command.rs`.
pub fn render_command_body(body: &str, arguments: &str) -> String {
    let tpl = body.replace("$ARGUMENTS", "{{ arguments }}");
    let mut ctx = tera::Context::new();
    ctx.insert("arguments", arguments);
    match tera::Tera::one_off(&tpl, &ctx, false) {
        Ok(rendered) => rendered,
        // Fall back to plain substitution so a literal `{{` in the body never
        // breaks command rendering. Both the new `{{ arguments }}` and any
        // remaining edge form resolve to the raw args here.
        Err(_) => tpl.replace("{{ arguments }}", arguments),
    }
}

/// Render a single user-role message. Used at history-insertion boundaries
/// where a built-in prompt becomes a `MessageContent::Text` block.
pub fn render_user_message<D: Serialize>(
    template: PromptTemplate,
    data: &D,
    cache: bool,
) -> anyhow::Result<LanguageModelRequestMessage> {
    Ok(LanguageModelRequestMessage {
        role: Role::User,
        content: vec![MessageContent::Text(render(template, data)?)],
        cache,
    })
}

/// Render a single message of an arbitrary role (e.g. the compaction preamble
/// rewrites a `Compaction` block into a `Text` block of the same role as the
/// carrying message).
pub fn render_message<D: Serialize>(
    role: Role,
    template: PromptTemplate,
    data: &D,
    cache: bool,
) -> anyhow::Result<LanguageModelRequestMessage> {
    Ok(LanguageModelRequestMessage {
        role,
        content: vec![MessageContent::Text(render(template, data)?)],
        cache,
    })
}

/// Render a tool definition at the `to_request_tools*` boundary. `description`
/// is rendered from a template; the JSON schema is passed through verbatim
/// (schema field descriptions are a separate concern — see Phase E notes).
pub fn render_tool(
    name: &str,
    description_template: PromptTemplate,
    description_data: &impl Serialize,
    input_schema: serde_json::Value,
    use_input_streaming: bool,
) -> anyhow::Result<LanguageModelRequestTool> {
    Ok(LanguageModelRequestTool {
        name: name.to_string(),
        description: render(description_template, description_data)?,
        input_schema,
        use_input_streaming,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_built_in_templates_parse_and_render_with_empty_context() {
        // Every template must (a) parse at startup and (b) render without
        // error against an empty context when it declares no required
        // variable. Templates that DO require a variable are exercised by
        // their own module tests; this loop only guards the static ones.
        let static_templates = [
            PromptTemplate::WrapperMaxTokensDirective,
            PromptTemplate::WrapperUnfulfilledToolIntentNudge,
            PromptTemplate::WrapperToolDenied,
            PromptTemplate::TitleFirstInstruction,
        ];
        for t in static_templates {
            let rendered = render_static(t).expect("template must render with empty context");
            assert!(!rendered.is_empty(), "{t:?} rendered empty");
            assert!(
                !rendered.contains("{{ "),
                "{t:?} left an unsubstituted variable: {rendered}"
            );
        }
    }

    #[test]
    fn every_variant_resolves_to_a_registered_template() {
        // Touch the global so `assert_all_registered` runs at init.
        let _ = render_static(PromptTemplate::WrapperMaxTokensDirective).unwrap();
        // `assert_all_registered` ties `ALL` to `REGISTRATIONS` (length +
        // per-variant parse) and `name()` is a compile-exhaustive match, so
        // the only thing left to guard is `ALL` itself staying exhaustive
        // over the enum. The count is hand-maintained and must be bumped
        // when a variant is added — this tripwire makes a forgotten bump
        // fail loudly here rather than letting a new variant ship
        // unregistered.
        assert_eq!(template::ALL.len(), 15);
        assert_eq!(REGISTRATIONS.len(), 15);
    }

    /// Every data-bearing template must fully substitute its variables against
    /// a representative payload, in both languages. Catches a data-struct
    /// field rename that leaves a template variable unsubstituted (which would
    /// otherwise leak `{{ x }}` into a model-facing prompt) at test time
    /// rather than in production.
    #[test]
    fn data_bearing_templates_render_without_leaked_syntax() {
        fn assert_clean(rendered: &str, t: PromptTemplate) {
            assert!(!rendered.is_empty(), "{t:?} rendered empty");
            assert!(
                !rendered.contains("{{") && !rendered.contains("{%"),
                "{t:?} left unsubstituted template syntax: {rendered}"
            );
        }

        {
            // System head.
            let main = crate::prompt::MainSystemPromptData {
                static_body: "STATIC",
                skills: vec![crate::prompt::SkillSummaryPromptData {
                    name: "n".to_string(),
                    description: "d".to_string(),
                }],
                language: crate::prompt::LanguagePromptData {
                    language: "English",
                },
                runtime: crate::prompt::RuntimeIdentityPromptData {
                    cwd: "/c".to_string(),
                    project: Some("/p".to_string()),
                    active_worktree: Some(crate::prompt::WorktreePromptData {
                        branch: "b".to_string(),
                        path: "/w".to_string(),
                    }),
                    os: "macos",
                    shell: "zsh".to_string(),
                    python3: "3.12".to_string(),
                    node: "20".to_string(),
                    today: "2026-07-14".to_string(),
                    permission_mode: "danger-full-access",
                },
                lsp_ready_specs: String::new(),
            };
            assert_clean(
                &render(PromptTemplate::SystemMain, &main).unwrap(),
                PromptTemplate::SystemMain,
            );

            let assembly = crate::prompt::SystemPromptAssembly {
                base: "BASE".to_string(),
                capabilities: crate::prompt::ModelCapabilitiesPromptData::default(),
                language: Some(crate::prompt::LanguagePromptData {
                    language: "English",
                }),
                worktree_subagent: Some(crate::prompt::WorktreePromptData {
                    branch: "b".to_string(),
                    path: "/w".to_string(),
                }),
            };
            assert_clean(
                &render(PromptTemplate::SystemAssembly, &assembly).unwrap(),
                PromptTemplate::SystemAssembly,
            );

            // Conversation wrappers.
            assert_clean(
                &render(
                    PromptTemplate::WrapperMaxTurnsSummary,
                    &crate::prompt::MaxTurnsSummaryData { max: 10 },
                )
                .unwrap(),
                PromptTemplate::WrapperMaxTurnsSummary,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperRecoveryFailure,
                    &crate::prompt::RecoveryFailureData {
                        reason: "boom".to_string(),
                    },
                )
                .unwrap(),
                PromptTemplate::WrapperRecoveryFailure,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperEmptyTurnNudge,
                    &crate::prompt::EmptyTurnNudgeData {},
                )
                .unwrap(),
                PromptTemplate::WrapperEmptyTurnNudge,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperDenialBreakerDirective,
                    &crate::prompt::DenialBreakerData { count: 5 },
                )
                .unwrap(),
                PromptTemplate::WrapperDenialBreakerDirective,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperPeerMessage,
                    &crate::prompt::PeerMessageData {
                        from: "x".to_string(),
                        content: "hi".to_string(),
                    },
                )
                .unwrap(),
                PromptTemplate::WrapperPeerMessage,
            );
            // Title topic-shift (uses a sentinel literal in data).
            assert_clean(
                &render(
                    PromptTemplate::TitleTopicShiftInstruction,
                    &crate::prompt::TopicShiftData {
                        current_title: "t".to_string(),
                        unchanged_sentinel: "UNCHANGED",
                    },
                )
                .unwrap(),
                PromptTemplate::TitleTopicShiftInstruction,
            );

            // Skill body (both branches: with/without description and arguments).
            assert_clean(
                &render(
                    PromptTemplate::SkillBody,
                    &crate::prompt::SkillBodyData {
                        description: Some("d".to_string()),
                        body: "body".to_string(),
                        arguments: Some("args".to_string()),
                    },
                )
                .unwrap(),
                PromptTemplate::SkillBody,
            );
            assert_clean(
                &render(
                    PromptTemplate::SkillBody,
                    &crate::prompt::SkillBodyData {
                        description: None,
                        body: "body".to_string(),
                        arguments: None,
                    },
                )
                .unwrap(),
                PromptTemplate::SkillBody,
            );
        }
    }

    /// Command bodies are untrusted prose rendered via the one-off path. A
    /// legacy `$ARGUMENTS` placeholder is rewritten; a literal `{{` in the body
    /// must fall back to plain substitution rather than erroring. Command
    /// bodies are intentionally not routed through the per-language registries.
    #[test]
    fn command_body_renders_arguments_and_falls_back_on_tera_syntax() {
        assert_eq!(
            render_command_body("Review $ARGUMENTS now", "HEAD~1"),
            "Review HEAD~1 now"
        );
        assert_eq!(
            render_command_body("See {{ arguments }} end", "x"),
            "See x end"
        );
        // Literal `{{` with no valid variable falls back to plain substitution.
        let broken = "Weird {{ thing body";
        assert_eq!(
            render_command_body(broken, "args"),
            broken.replace("{{ arguments }}", "args")
        );
    }

    /// W1 byte-freeze: the exact bytes plan mode puts in front of the model.
    /// The active briefing is re-injected every turn, so any drift re-bills the
    /// prompt; a later work package may only move these bytes by updating what
    /// is pinned here.
    #[test]
    fn plan_mode_templates_bytes_are_frozen() {
        // Active briefing: the embedded template is the frozen prose and the
        // only dynamic seam is `plans_dir`, which must appear exactly four
        // times and nowhere else. A renderer that adds, trims or rewrites bytes
        // around the substitution fails here even though the prose itself is
        // maintained in the `.tera.md`.
        let active = crate::collaboration_mode::render_plan_mode_active("/p/plans")
            .expect("plan-mode briefing renders");
        assert_eq!(
            active,
            TPL_PLAN_MODE_ACTIVE.replace("{{ plans_dir }}", "/p/plans"),
            "the active briefing drifted from its template, or the placeholder vocabulary moved"
        );
        assert_eq!(
            active.matches("/p/plans").count(),
            4,
            "exactly the four declared plan-dir references"
        );
        assert!(
            active.starts_with("<critical>\nPlan mode is active."),
            "opening bytes are frozen: {}",
            &active[..40]
        );
        assert!(
            active.ends_with(
                "You MUST keep going until the plan is decision-complete.\n</critical>\n"
            ),
            "closing bytes (including the trailing newline) are frozen"
        );

        let approved = crate::collaboration_mode::render_plan_mode_approved("/p/auth-plan.md")
            .expect("approved briefing renders");
        assert_eq!(
            approved,
            r##"The plan at `/p/auth-plan.md` has been approved by the user. Read the plan file, then implement it top to bottom exactly as written:

- The plan is decision-complete — execute it, do NOT re-plan, re-design, or reopen settled choices.
- If a step is ambiguous in a way the plan could not have anticipated, pick the smallest interpretation consistent with the plan's Context and Verification sections, and note the choice in your final summary.
- Verify each load-bearing step as the plan's Verification section prescribes before reporting done.
- Publish and track your execution progress with `UpdatePlan`: right after starting, publish the complete step list, then update it whenever progress changes (mark steps completed as you finish, keep at most one in_progress, all completed before you end). This drives the plan overview shown to the user.
"##
        );
    }

    /// W1 byte-freeze: one representative main system-prompt render with every
    /// branch live (skills block, LSP line, project and worktree rows). This is
    /// the head of the cached prefix of every request; the bytes are pinned
    /// exactly, whitespace-control newlines included.
    #[test]
    fn main_system_prompt_bytes_are_frozen() {
        let main = crate::prompt::MainSystemPromptData {
            static_body: "STATIC",
            skills: vec![crate::prompt::SkillSummaryPromptData {
                name: "n".to_string(),
                description: "d".to_string(),
            }],
            language: crate::prompt::LanguagePromptData {
                language: "English",
            },
            runtime: crate::prompt::RuntimeIdentityPromptData {
                cwd: "/c".to_string(),
                project: Some("/p".to_string()),
                active_worktree: Some(crate::prompt::WorktreePromptData {
                    branch: "b".to_string(),
                    path: "/w".to_string(),
                }),
                os: "macos",
                shell: "zsh".to_string(),
                python3: "3.12".to_string(),
                node: "20".to_string(),
                today: "2026-07-14".to_string(),
                permission_mode: "danger-full-access",
            },
            lsp_ready_specs: "rust-analyzer".to_string(),
        };
        assert_eq!(
            render(PromptTemplate::SystemMain, &main).unwrap(),
            r##"STATIC

## Available skills (consult their full body via the `skill` tool on demand)
- n: d


## LSP ready
rust-analyzer

## Tool preferences
Prefer Grep/Glob/Ls over raw grep/find/ls in Bash — no sandbox, no approval in read-only mode, bounded structured output. Use Bash shell commands only when the tool's feature set is insufficient (pipes, complex flags, chained commands).

## Concurrency model
Foreground tool calls (Bash without `run_in_background`) block this turn. Background Bash (`run_in_background: true`) returns immediately and wakes the idle session on completion — never use `sleep` or poll loops to wait for a background task. `Monitor` streams events continuously for long-running observation (log tail, event stream). Use `BashOutput` to fetch full output and `TaskStop` to cancel.

## Language

Unless the user specifies otherwise, write your user-facing responses in English.

## Runtime identity

- Current working directory: `/c`
- Project root: `/p`
- Active worktree: `b` at `/w`
- Operating system: macos
- Default shell: zsh
- python3: 3.12
- node: 20
- Today: 2026-07-14
- Permission mode: danger-full-access. Modes: read-only (bash runs but writes are denied by the seatbelt; fs mutations refused), workspace-write (writes under the workspace, the manox home (~/.manox), and temp areas; bash confined to the workspace-write profile), danger-full-access (no sandbox; bash unsandboxed, fs mutations unfenced). A denied bash or fs write is reported as `[sandbox: file access denied under <mode> mode]`; when a wider mode would let it succeed, retry the exact same call once with `sandbox_permissions` (the narrowest wider mode that suffices) + a one-sentence `justification` — the approval prompt asks the user. Never escalate speculatively.
"##
        );
    }
}
