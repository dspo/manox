//! Single Tera instance + the only place outside tests that touches `tera::`.
//!
//! Every model-visible prompt is rendered here. The module boundary rule:
//! callers pass a [`PromptTemplate`] key + a [`Serialize`] payload and get a
//! `String` back — they never construct a `tera::Tera` / `tera::Context`
//! themselves. One process-global [`Tera`] (lazily initialized via
//! [`OnceLock`]) registers every built-in `.tera.md` embedded at compile time,
//! so the parse cost is paid once and the registry is immutable thereafter.
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

// Compile-time-embedded template sources. Each `include_str!` path mirrors
// the [`PromptTemplate::name`] registry key, so a missing file fails the build
// rather than surfacing as a runtime render error.
const TPL_SYSTEM_MAIN: &str = include_str!("templates/system/main.tera.md");
const TPL_SYSTEM_ASSEMBLY: &str = include_str!("templates/system/assembly.tera.md");
const TPL_MODE_GOAL: &str = include_str!("templates/mode/goal.tera.md");
const TPL_MODE_ULTRACODE: &str = include_str!("templates/mode/ultracode.tera.md");
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
const TPL_WRAPPER_PEER_MESSAGE: &str = include_str!("templates/wrapper/peer_message.tera.md");
const TPL_WRAPPER_ASK_USER_QUESTIONS: &str =
    include_str!("templates/wrapper/ask_user_questions.tera.md");
const TPL_WRAPPER_TOOL_DENIED: &str = include_str!("templates/wrapper/tool_denied.tera.md");
const TPL_WRAPPER_GOAL_CONTINUATION: &str =
    include_str!("templates/wrapper/goal_continuation.tera.md");
const TPL_WRAPPER_COMPACTION_PREAMBLE: &str =
    include_str!("templates/wrapper/compaction_preamble.tera.md");
const TPL_SIDECALL_APPROVAL_SYSTEM: &str =
    include_str!("templates/side_call/approval_system.tera.md");
const TPL_SIDECALL_APPROVAL_USER: &str = include_str!("templates/side_call/approval_user.tera.md");
const TPL_SIDECALL_GOAL_SYSTEM: &str = include_str!("templates/side_call/goal_system.tera.md");
const TPL_SIDECALL_GOAL_USER: &str = include_str!("templates/side_call/goal_user.tera.md");
const TPL_SIDECALL_COMPACT_SYSTEM: &str =
    include_str!("templates/side_call/compact_system.tera.md");
const TPL_SIDECALL_COMPACT_FINAL: &str = include_str!("templates/side_call/compact_final.tera.md");
const TPL_TITLE_FIRST: &str = include_str!("templates/title/first.tera.md");
const TPL_TITLE_TOPIC_SHIFT: &str = include_str!("templates/title/topic_shift.tera.md");
const TPL_SKILL_BODY: &str = include_str!("templates/wrapper/skill_body.tera.md");
const TPL_AGENT_TOOL: &str = include_str!("templates/tools/agent_tool.tera.md");

/// The lazily-initialized global Tera registry. Holds every built-in template
/// parsed once; immutable for the process lifetime after first use.
fn tera() -> &'static tera::Tera {
    static TERA: OnceLock<tera::Tera> = OnceLock::new();
    TERA.get_or_init(|| {
        let mut tera = tera::Tera::default();
        // Autoescape is off by default for non-HTML template extensions; the
        // `.tera.md` templates are model-facing markdown, so `{{ var }}`
        // inserts raw bytes. (Variables are never re-parsed as template
        // syntax, so a `static_body` carrying literal `{{` is safe.)
        // One (variant, embedded source) pair per built-in template. Adding a
        // prompt = add a variant to `PromptTemplate` + `ALL`, a `TPL_*` const,
        // and a row here. A missing row panics at first use (see
        // `assert_all_registered`) rather than at a deferred render site.
        // Tera resolves `{% include %}` at `add_raw_template` time, so the
        // `mode/*` targets must be registered before `system/assembly.tera.md`
        // (which includes them) is parsed. Registration order otherwise does
        // not matter.
        let registrations: &[(PromptTemplate, &str)] = &[
            (PromptTemplate::SystemMain, TPL_SYSTEM_MAIN),
            (PromptTemplate::ModeGoalAddendum, TPL_MODE_GOAL),
            (PromptTemplate::ModeUltracodeGrant, TPL_MODE_ULTRACODE),
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
            (PromptTemplate::WrapperPeerMessage, TPL_WRAPPER_PEER_MESSAGE),
            (
                PromptTemplate::WrapperAskUserQuestions,
                TPL_WRAPPER_ASK_USER_QUESTIONS,
            ),
            (PromptTemplate::WrapperToolDenied, TPL_WRAPPER_TOOL_DENIED),
            (
                PromptTemplate::WrapperGoalContinuation,
                TPL_WRAPPER_GOAL_CONTINUATION,
            ),
            (
                PromptTemplate::WrapperCompactionPreamble,
                TPL_WRAPPER_COMPACTION_PREAMBLE,
            ),
            (
                PromptTemplate::SideCallApprovalSystem,
                TPL_SIDECALL_APPROVAL_SYSTEM,
            ),
            (
                PromptTemplate::SideCallApprovalUser,
                TPL_SIDECALL_APPROVAL_USER,
            ),
            (PromptTemplate::SideCallGoalSystem, TPL_SIDECALL_GOAL_SYSTEM),
            (PromptTemplate::SideCallGoalUser, TPL_SIDECALL_GOAL_USER),
            (
                PromptTemplate::SideCallCompactSystem,
                TPL_SIDECALL_COMPACT_SYSTEM,
            ),
            (
                PromptTemplate::SideCallCompactFinalInstruction,
                TPL_SIDECALL_COMPACT_FINAL,
            ),
            (PromptTemplate::TitleFirstInstruction, TPL_TITLE_FIRST),
            (
                PromptTemplate::TitleTopicShiftInstruction,
                TPL_TITLE_TOPIC_SHIFT,
            ),
            (PromptTemplate::SkillBody, TPL_SKILL_BODY),
            (PromptTemplate::AgentToolDescription, TPL_AGENT_TOOL),
        ];
        for (variant, src) in registrations {
            tera.add_raw_template(variant.name(), src)
                .unwrap_or_else(|e| {
                    panic!("built-in prompt template {variant:?} failed to parse: {e}")
                });
        }
        assert_all_registered(&tera, registrations);
        tera
    })
}

/// Every [`PromptTemplate`] variant (per [`template::ALL`]) must have a
/// registered source. Catches "added a variant, forgot the `TPL_*` const or
/// the registration row" at first use rather than at the deferred render site.
fn assert_all_registered(tera: &tera::Tera, registrations: &[(PromptTemplate, &str)]) {
    // `ALL` and the registration table are both hand-maintained, so a variant
    // added to one but not the other would slip past the per-variant checks
    // below (which only iterate `ALL`). Tie their lengths here so that drift
    // panics at first use instead of silently leaving a variant unrenderable.
    assert_eq!(
        template::ALL.len(),
        registrations.len(),
        "template::ALL ({} entries) and the registration table ({} rows) drifted \
         out of sync — a variant was added to one but not the other",
        template::ALL.len(),
        registrations.len(),
    );
    let registered: std::collections::HashSet<&str> =
        registrations.iter().map(|(v, _)| v.name()).collect();
    let parsed: std::collections::HashSet<&str> = tera.get_template_names().collect();
    for variant in template::ALL {
        let name = variant.name();
        assert!(
            registered.contains(name),
            "PromptTemplate variant `{name}` has no registration row"
        );
        assert!(
            parsed.contains(name),
            "PromptTemplate variant `{name}` registered but not parsed into Tera"
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
/// — they are rendered via Tera's one-off path against the live body string.
/// For backwards compatibility the legacy `$ARGUMENTS` placeholder is
/// rewritten to `{{ arguments }}` first, so old command files keep working
/// without a rewrite. If the body contains Tera-incompatible literal syntax
/// (an unmatched `{%` / `{{`, or an unknown variable), the one-off render
/// fails and the function falls back to a plain string substitution — command
/// bodies are untrusted prose, and a literal `{{` must never break a command.
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
            PromptTemplate::ModeGoalAddendum,
            PromptTemplate::ModeUltracodeGrant,
            PromptTemplate::WrapperMaxTokensDirective,
            PromptTemplate::WrapperUnfulfilledToolIntentNudge,
            PromptTemplate::WrapperToolDenied,
            PromptTemplate::SideCallApprovalSystem,
            PromptTemplate::SideCallGoalSystem,
            PromptTemplate::SideCallCompactSystem,
            PromptTemplate::SideCallCompactFinalInstruction,
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
        // Touch the global so `assert_all_registered` runs at init. A variant
        // lacking a template file panics there rather than at render time.
        let _ = render_static(PromptTemplate::ModeGoalAddendum).unwrap();
        // `assert_all_registered` ties `ALL` to the registration table (length +
        // per-variant registration) and `name()` is a compile-exhaustive match,
        // so the only thing left to guard is `ALL` itself staying exhaustive
        // over the enum. The count is hand-maintained and must be bumped when a
        // variant is added — this tripwire makes a forgotten bump fail loudly
        // here rather than letting a new variant ship unregistered.
        assert_eq!(template::ALL.len(), 24);
    }

    /// Every data-bearing template must fully substitute its variables against
    /// a representative payload. Catches a data-struct field rename that leaves
    /// a template variable unsubstituted (which would otherwise leak `{{ x }}`
    /// into a model-facing prompt) at test time rather than in production.
    #[test]
    fn data_bearing_templates_render_without_leaked_syntax() {
        fn assert_clean(rendered: &str, t: PromptTemplate) {
            assert!(!rendered.is_empty(), "{t:?} rendered empty");
            assert!(
                !rendered.contains("{{") && !rendered.contains("{%"),
                "{t:?} left unsubstituted template syntax: {rendered}"
            );
        }

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
                approval_mode: Some("Yolo"),
            },
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
            goal: true,
            ultracode: true,
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
                &crate::prompt::EmptyTurnNudgeData { in_plan: true },
            )
            .unwrap(),
            PromptTemplate::WrapperEmptyTurnNudge,
        );
        assert_clean(
            &render(
                PromptTemplate::WrapperEmptyTurnNudge,
                &crate::prompt::EmptyTurnNudgeData { in_plan: false },
            )
            .unwrap(),
            PromptTemplate::WrapperEmptyTurnNudge,
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
        assert_clean(
            &render(
                PromptTemplate::WrapperAskUserQuestions,
                &crate::prompt::AskUserQuestionsData {
                    answers: vec![crate::prompt::AskUserQa {
                        question: "q".to_string(),
                        answer: "a".to_string(),
                    }],
                    response: Some("extra context".to_string()),
                },
            )
            .unwrap(),
            PromptTemplate::WrapperAskUserQuestions,
        );
        assert_clean(
            &render(
                PromptTemplate::WrapperGoalContinuation,
                &crate::prompt::GoalContinuationData {
                    condition: "c".to_string(),
                },
            )
            .unwrap(),
            PromptTemplate::WrapperGoalContinuation,
        );
        assert_clean(
            &render(
                PromptTemplate::WrapperCompactionPreamble,
                &crate::prompt::CompactionPreambleData {
                    summary: "s".to_string(),
                },
            )
            .unwrap(),
            PromptTemplate::WrapperCompactionPreamble,
        );

        // Side-call user prompts.
        assert_clean(
            &render(
                PromptTemplate::SideCallApprovalUser,
                &crate::prompt::ApprovalReviewPromptData {
                    cwd: "/c".to_string(),
                    tool_name: "Bash".to_string(),
                    tool_title: "Bash".to_string(),
                    tool_input: "{}".to_string(),
                },
            )
            .unwrap(),
            PromptTemplate::SideCallApprovalUser,
        );
        assert_clean(
            &render(
                PromptTemplate::SideCallGoalUser,
                &crate::prompt::GoalEvalPromptData {
                    condition: "c".to_string(),
                    last_user: "u".to_string(),
                    last_assistant: "a".to_string(),
                },
            )
            .unwrap(),
            PromptTemplate::SideCallGoalUser,
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

        // Agent tool description (both branches: with/without subagents).
        assert_clean(
            &render(
                PromptTemplate::AgentToolDescription,
                &crate::prompt::AgentToolDescriptionData {
                    subagents: vec![crate::prompt::SubagentTypeData {
                        name: "plan".to_string(),
                        capability: "read-only",
                        description: "plans".to_string(),
                    }],
                },
            )
            .unwrap(),
            PromptTemplate::AgentToolDescription,
        );
        assert_clean(
            &render(
                PromptTemplate::AgentToolDescription,
                &crate::prompt::AgentToolDescriptionData { subagents: vec![] },
            )
            .unwrap(),
            PromptTemplate::AgentToolDescription,
        );
    }

    /// Command bodies are untrusted prose rendered via the one-off path. A
    /// legacy `$ARGUMENTS` placeholder is rewritten; a literal `{{` in the body
    /// must fall back to plain substitution rather than erroring.
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
}
