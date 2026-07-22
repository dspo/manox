//! One Tera instance per agent language + the only place outside tests that
//! touches `tera::`.
//!
//! Every model-visible prompt is rendered here. The module boundary rule:
//! callers pass a [`PromptTemplate`] key + a [`Language`] + a [`Serialize`]
//! payload and get a `String` back — they never construct a `tera::Tera` /
//! `tera::Context` themselves. One process-global [`Tera`] per agent language
//! (lazily initialized via [`OnceLock`]) registers every built-in `.tera.md`
//! embedded at compile time, so the parse cost is paid once per language and
//! the registries are immutable thereafter.
//!
//! The language axis is the thread's immutable [`Language`] (not the
//! process-global UI locale), so two threads in different agent languages
//! render distinct prose concurrently against separate Tera instances.
//!
//! Autoescape is off: these are model-facing markdown/text prompts, not HTML,
//! so `{{ var }}` inserts raw bytes. Variables are inserted as opaque text
//! (never re-parsed as template syntax), so a `static_body` carrying literal
//! `{{` is safe.

use std::sync::OnceLock;

use serde::Serialize;

use crate::language::Language;
use crate::language_model::{
    LanguageModelRequestMessage, LanguageModelRequestTool, MessageContent, Role,
};
use crate::prompt::template::{self, PromptTemplate};

// Compile-time-embedded template sources, mirrored per agent language. Each
// `(variant, en, zh-CN)` triple mirrors the [`PromptTemplate::name`] registry
// key, so a missing file in either language fails the build rather than
// surfacing as a runtime render error.
const TPL_SYSTEM_MAIN_EN: &str = include_str!("templates/en/system/main.tera.md");
const TPL_SYSTEM_MAIN_ZH_CN: &str = include_str!("templates/zh-CN/system/main.tera.md");
const TPL_SYSTEM_ASSEMBLY_EN: &str = include_str!("templates/en/system/assembly.tera.md");
const TPL_SYSTEM_ASSEMBLY_ZH_CN: &str = include_str!("templates/zh-CN/system/assembly.tera.md");
const TPL_MODE_GOAL_EN: &str = include_str!("templates/en/mode/goal.tera.md");
const TPL_MODE_GOAL_ZH_CN: &str = include_str!("templates/zh-CN/mode/goal.tera.md");
const TPL_WRAPPER_MAX_TURNS_SUMMARY_EN: &str =
    include_str!("templates/en/wrapper/max_turns_summary.tera.md");
const TPL_WRAPPER_MAX_TURNS_SUMMARY_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/max_turns_summary.tera.md");
const TPL_WRAPPER_MAX_TOKENS_DIRECTIVE_EN: &str =
    include_str!("templates/en/wrapper/max_tokens_directive.tera.md");
const TPL_WRAPPER_MAX_TOKENS_DIRECTIVE_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/max_tokens_directive.tera.md");
const TPL_WRAPPER_RECOVERY_FAILURE_EN: &str =
    include_str!("templates/en/wrapper/recovery_failure.tera.md");
const TPL_WRAPPER_RECOVERY_FAILURE_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/recovery_failure.tera.md");
const TPL_WRAPPER_EMPTY_TURN_NUDGE_EN: &str =
    include_str!("templates/en/wrapper/empty_turn_nudge.tera.md");
const TPL_WRAPPER_EMPTY_TURN_NUDGE_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/empty_turn_nudge.tera.md");
const TPL_WRAPPER_UNFULFILLED_TOOL_INTENT_NUDGE_EN: &str =
    include_str!("templates/en/wrapper/unfulfilled_tool_intent_nudge.tera.md");
const TPL_WRAPPER_UNFULFILLED_TOOL_INTENT_NUDGE_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/unfulfilled_tool_intent_nudge.tera.md");
const TPL_WRAPPER_PEER_MESSAGE_EN: &str = include_str!("templates/en/wrapper/peer_message.tera.md");
const TPL_WRAPPER_PEER_MESSAGE_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/peer_message.tera.md");
const TPL_WRAPPER_ASK_USER_QUESTIONS_EN: &str =
    include_str!("templates/en/wrapper/ask_user_questions.tera.md");
const TPL_WRAPPER_ASK_USER_QUESTIONS_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/ask_user_questions.tera.md");
const TPL_WRAPPER_TOOL_DENIED_EN: &str = include_str!("templates/en/wrapper/tool_denied.tera.md");
const TPL_WRAPPER_TOOL_DENIED_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/tool_denied.tera.md");
const TPL_WRAPPER_GOAL_CONTINUATION_EN: &str =
    include_str!("templates/en/wrapper/goal_continuation.tera.md");
const TPL_WRAPPER_GOAL_CONTINUATION_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/goal_continuation.tera.md");
const TPL_WRAPPER_COMPACTION_PREAMBLE_EN: &str =
    include_str!("templates/en/wrapper/compaction_preamble.tera.md");
const TPL_WRAPPER_COMPACTION_PREAMBLE_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/compaction_preamble.tera.md");
const TPL_WRAPPER_INSTRUCTIONS_EAGER_EN: &str =
    include_str!("templates/en/wrapper/instructions_eager.tera.md");
const TPL_WRAPPER_INSTRUCTIONS_EAGER_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/instructions_eager.tera.md");
const TPL_WRAPPER_INSTRUCTIONS_LAZY_EN: &str =
    include_str!("templates/en/wrapper/instructions_lazy.tera.md");
const TPL_WRAPPER_INSTRUCTIONS_LAZY_ZH_CN: &str =
    include_str!("templates/zh-CN/wrapper/instructions_lazy.tera.md");
const TPL_SIDECALL_APPROVAL_SYSTEM_EN: &str =
    include_str!("templates/en/side_call/approval_system.tera.md");
const TPL_SIDECALL_APPROVAL_SYSTEM_ZH_CN: &str =
    include_str!("templates/zh-CN/side_call/approval_system.tera.md");
const TPL_SIDECALL_APPROVAL_USER_EN: &str =
    include_str!("templates/en/side_call/approval_user.tera.md");
const TPL_SIDECALL_APPROVAL_USER_ZH_CN: &str =
    include_str!("templates/zh-CN/side_call/approval_user.tera.md");
const TPL_SIDECALL_GOAL_SYSTEM_EN: &str =
    include_str!("templates/en/side_call/goal_system.tera.md");
const TPL_SIDECALL_GOAL_SYSTEM_ZH_CN: &str =
    include_str!("templates/zh-CN/side_call/goal_system.tera.md");
const TPL_SIDECALL_GOAL_USER_EN: &str = include_str!("templates/en/side_call/goal_user.tera.md");
const TPL_SIDECALL_GOAL_USER_ZH_CN: &str =
    include_str!("templates/zh-CN/side_call/goal_user.tera.md");
const TPL_SIDECALL_COMPACT_SYSTEM_EN: &str =
    include_str!("templates/en/side_call/compact_system.tera.md");
const TPL_SIDECALL_COMPACT_SYSTEM_ZH_CN: &str =
    include_str!("templates/zh-CN/side_call/compact_system.tera.md");
const TPL_SIDECALL_COMPACT_FINAL_EN: &str =
    include_str!("templates/en/side_call/compact_final.tera.md");
const TPL_SIDECALL_COMPACT_FINAL_ZH_CN: &str =
    include_str!("templates/zh-CN/side_call/compact_final.tera.md");
const TPL_TITLE_FIRST_EN: &str = include_str!("templates/en/title/first.tera.md");
const TPL_TITLE_FIRST_ZH_CN: &str = include_str!("templates/zh-CN/title/first.tera.md");
const TPL_TITLE_TOPIC_SHIFT_EN: &str = include_str!("templates/en/title/topic_shift.tera.md");
const TPL_TITLE_TOPIC_SHIFT_ZH_CN: &str = include_str!("templates/zh-CN/title/topic_shift.tera.md");
const TPL_SKILL_BODY_EN: &str = include_str!("templates/en/wrapper/skill_body.tera.md");
const TPL_SKILL_BODY_ZH_CN: &str = include_str!("templates/zh-CN/wrapper/skill_body.tera.md");
const TPL_AGENT_TOOL_EN: &str = include_str!("templates/en/tools/agent_tool.tera.md");
const TPL_AGENT_TOOL_ZH_CN: &str = include_str!("templates/zh-CN/tools/agent_tool.tera.md");

/// `(PromptTemplate, English source, 简体中文 source)` for every built-in
/// template. The single source of truth for what gets parsed into each
/// language's Tera. Order matters: `mode/goal.tera.md` must be registered
/// before `system/assembly.tera.md` (which `{% include %}`s it), because Tera
/// resolves includes at `add_raw_template` time.
const REGISTRATIONS: &[(PromptTemplate, &str, &str)] = &[
    (
        PromptTemplate::SystemMain,
        TPL_SYSTEM_MAIN_EN,
        TPL_SYSTEM_MAIN_ZH_CN,
    ),
    (
        PromptTemplate::ModeGoalAddendum,
        TPL_MODE_GOAL_EN,
        TPL_MODE_GOAL_ZH_CN,
    ),
    (
        PromptTemplate::SystemAssembly,
        TPL_SYSTEM_ASSEMBLY_EN,
        TPL_SYSTEM_ASSEMBLY_ZH_CN,
    ),
    (
        PromptTemplate::WrapperMaxTurnsSummary,
        TPL_WRAPPER_MAX_TURNS_SUMMARY_EN,
        TPL_WRAPPER_MAX_TURNS_SUMMARY_ZH_CN,
    ),
    (
        PromptTemplate::WrapperMaxTokensDirective,
        TPL_WRAPPER_MAX_TOKENS_DIRECTIVE_EN,
        TPL_WRAPPER_MAX_TOKENS_DIRECTIVE_ZH_CN,
    ),
    (
        PromptTemplate::WrapperRecoveryFailure,
        TPL_WRAPPER_RECOVERY_FAILURE_EN,
        TPL_WRAPPER_RECOVERY_FAILURE_ZH_CN,
    ),
    (
        PromptTemplate::WrapperEmptyTurnNudge,
        TPL_WRAPPER_EMPTY_TURN_NUDGE_EN,
        TPL_WRAPPER_EMPTY_TURN_NUDGE_ZH_CN,
    ),
    (
        PromptTemplate::WrapperUnfulfilledToolIntentNudge,
        TPL_WRAPPER_UNFULFILLED_TOOL_INTENT_NUDGE_EN,
        TPL_WRAPPER_UNFULFILLED_TOOL_INTENT_NUDGE_ZH_CN,
    ),
    (
        PromptTemplate::WrapperPeerMessage,
        TPL_WRAPPER_PEER_MESSAGE_EN,
        TPL_WRAPPER_PEER_MESSAGE_ZH_CN,
    ),
    (
        PromptTemplate::WrapperAskUserQuestions,
        TPL_WRAPPER_ASK_USER_QUESTIONS_EN,
        TPL_WRAPPER_ASK_USER_QUESTIONS_ZH_CN,
    ),
    (
        PromptTemplate::WrapperToolDenied,
        TPL_WRAPPER_TOOL_DENIED_EN,
        TPL_WRAPPER_TOOL_DENIED_ZH_CN,
    ),
    (
        PromptTemplate::WrapperGoalContinuation,
        TPL_WRAPPER_GOAL_CONTINUATION_EN,
        TPL_WRAPPER_GOAL_CONTINUATION_ZH_CN,
    ),
    (
        PromptTemplate::WrapperCompactionPreamble,
        TPL_WRAPPER_COMPACTION_PREAMBLE_EN,
        TPL_WRAPPER_COMPACTION_PREAMBLE_ZH_CN,
    ),
    (
        PromptTemplate::WrapperInstructionsEager,
        TPL_WRAPPER_INSTRUCTIONS_EAGER_EN,
        TPL_WRAPPER_INSTRUCTIONS_EAGER_ZH_CN,
    ),
    (
        PromptTemplate::WrapperInstructionsLazy,
        TPL_WRAPPER_INSTRUCTIONS_LAZY_EN,
        TPL_WRAPPER_INSTRUCTIONS_LAZY_ZH_CN,
    ),
    (
        PromptTemplate::SideCallApprovalSystem,
        TPL_SIDECALL_APPROVAL_SYSTEM_EN,
        TPL_SIDECALL_APPROVAL_SYSTEM_ZH_CN,
    ),
    (
        PromptTemplate::SideCallApprovalUser,
        TPL_SIDECALL_APPROVAL_USER_EN,
        TPL_SIDECALL_APPROVAL_USER_ZH_CN,
    ),
    (
        PromptTemplate::SideCallGoalSystem,
        TPL_SIDECALL_GOAL_SYSTEM_EN,
        TPL_SIDECALL_GOAL_SYSTEM_ZH_CN,
    ),
    (
        PromptTemplate::SideCallGoalUser,
        TPL_SIDECALL_GOAL_USER_EN,
        TPL_SIDECALL_GOAL_USER_ZH_CN,
    ),
    (
        PromptTemplate::SideCallCompactSystem,
        TPL_SIDECALL_COMPACT_SYSTEM_EN,
        TPL_SIDECALL_COMPACT_SYSTEM_ZH_CN,
    ),
    (
        PromptTemplate::SideCallCompactFinalInstruction,
        TPL_SIDECALL_COMPACT_FINAL_EN,
        TPL_SIDECALL_COMPACT_FINAL_ZH_CN,
    ),
    (
        PromptTemplate::TitleFirstInstruction,
        TPL_TITLE_FIRST_EN,
        TPL_TITLE_FIRST_ZH_CN,
    ),
    (
        PromptTemplate::TitleTopicShiftInstruction,
        TPL_TITLE_TOPIC_SHIFT_EN,
        TPL_TITLE_TOPIC_SHIFT_ZH_CN,
    ),
    (
        PromptTemplate::SkillBody,
        TPL_SKILL_BODY_EN,
        TPL_SKILL_BODY_ZH_CN,
    ),
    (
        PromptTemplate::AgentToolDescription,
        TPL_AGENT_TOOL_EN,
        TPL_AGENT_TOOL_ZH_CN,
    ),
];

/// The lazily-initialized global Tera registry for `lang`. Holds every built-in
/// template parsed once per language; immutable for the process lifetime after
/// first use. Two independent instances keep English and 简体中文 threads from
/// contending on the same registry while letting each render in its own prose.
fn tera_for(lang: Language) -> &'static tera::Tera {
    static TERA_EN: OnceLock<tera::Tera> = OnceLock::new();
    static TERA_ZH_CN: OnceLock<tera::Tera> = OnceLock::new();
    match lang {
        Language::En => TERA_EN.get_or_init(|| build_tera(Language::En)),
        Language::ZhCn => TERA_ZH_CN.get_or_init(|| build_tera(Language::ZhCn)),
    }
}

/// Parse every registered template source for `lang` into a fresh [`Tera`].
/// One (variant, source) pair per built-in template, selected by `lang`. A
/// parse failure panics at first use (see [`assert_all_registered`]) rather
/// than at a deferred render site. Tera resolves `{% include %}` at
/// `add_raw_template` time, so the `mode/*` targets must be registered before
/// `system/assembly.tera.md` (which includes them) is parsed — the
/// [`REGISTRATIONS`] order already guarantees that.
fn build_tera(lang: Language) -> tera::Tera {
    let mut tera = tera::Tera::default();
    // Autoescape is off by default for non-HTML template extensions; the
    // `.tera.md` templates are model-facing markdown, so `{{ var }}`
    // inserts raw bytes. (Variables are never re-parsed as template
    // syntax, so a `static_body` carrying literal `{{` is safe.)
    for (variant, en_src, zh_src) in REGISTRATIONS {
        let src = match lang {
            Language::En => en_src,
            Language::ZhCn => zh_src,
        };
        tera.add_raw_template(variant.name(), src)
            .unwrap_or_else(|e| {
                panic!("built-in prompt template {variant:?} ({lang:?}) failed to parse: {e}")
            });
    }
    assert_all_registered(&tera, lang);
    tera
}

/// Every [`PromptTemplate`] variant (per [`template::ALL`]) must have a
/// registered, parsed source in this language's Tera. Catches "added a
/// variant, forgot the `TPL_*` const or the registration row, in one language
/// only" at first use of that language rather than at a deferred render site.
fn assert_all_registered(tera: &tera::Tera, _lang: Language) {
    // `ALL`, `REGISTRATIONS`, and each language's `TPL_*` set are all
    // hand-maintained. A variant added to one but not the others would slip
    // past the per-variant checks below — tie their lengths here so drift
    // panics at first use instead of silently leaving a variant unrenderable
    // in one language.
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
            "PromptTemplate variant `{name}` is not parsed into the Tera for {_lang:?} \
             — its source const or registration row is missing for that language"
        );
    }
}

/// Render `template` in `lang` with `data`. The single materialize entry
/// point: every model-visible prompt string is produced here. `lang` is the
/// thread's immutable agent language — never the process-global UI locale —
/// so two threads in different agent languages render distinct prose
/// concurrently. Returns the rendered text; errors surface as `anyhow::Error`
/// so the boundary can `?`-propagate.
///
/// For templates with no variables, pass `&()`.
pub fn render<D: Serialize>(
    template: PromptTemplate,
    lang: Language,
    data: &D,
) -> anyhow::Result<String> {
    let tera = tera_for(lang);
    let ctx = tera::Context::from_serialize(data)?;
    Ok(tera.render(template.name(), &ctx)?)
}

/// Render a no-variable template in `lang`. Convenience for static prose (mode
/// addendums, side-call system prompts) that carries no payload.
pub fn render_static(template: PromptTemplate, lang: Language) -> anyhow::Result<String> {
    render(
        template,
        lang,
        &std::collections::HashMap::<&str, &str>::new(),
    )
}

/// Render a slash-command body, substituting `arguments` into the
/// `{{ arguments }}` placeholder.
///
/// Command bodies are loaded from disk at runtime (user / plugin-authored),
/// so unlike the built-in compile-time templates they cannot be pre-registered
/// — they are rendered via Tera's one-off path against the live body string,
/// and intentionally not routed through the per-language registries: a command
/// body is untrusted prose authored in whatever language its author chose, not
/// a manox-maintained bilingual asset. For backwards compatibility the legacy
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

/// Render a single user-role message in `lang`. Used at history-insertion
/// boundaries where a built-in prompt becomes a `MessageContent::Text` block.
pub fn render_user_message<D: Serialize>(
    template: PromptTemplate,
    lang: Language,
    data: &D,
    cache: bool,
) -> anyhow::Result<LanguageModelRequestMessage> {
    Ok(LanguageModelRequestMessage {
        role: Role::User,
        content: vec![MessageContent::Text(render(template, lang, data)?)],
        cache,
    })
}

/// Render a single message of an arbitrary role (e.g. the compaction preamble
/// rewrites a `Compaction` block into a `Text` block of the same role as the
/// carrying message).
pub fn render_message<D: Serialize>(
    role: Role,
    template: PromptTemplate,
    lang: Language,
    data: &D,
    cache: bool,
) -> anyhow::Result<LanguageModelRequestMessage> {
    Ok(LanguageModelRequestMessage {
        role,
        content: vec![MessageContent::Text(render(template, lang, data)?)],
        cache,
    })
}

/// Render a tool definition at the `to_request_tools*` boundary. `description`
/// is rendered from a template in `lang`; the JSON schema is passed through
/// verbatim (schema field descriptions are a separate concern — see Phase E
/// notes).
pub fn render_tool(
    name: &str,
    description_template: PromptTemplate,
    lang: Language,
    description_data: &impl Serialize,
    input_schema: serde_json::Value,
    use_input_streaming: bool,
) -> anyhow::Result<LanguageModelRequestTool> {
    Ok(LanguageModelRequestTool {
        name: name.to_string(),
        description: render(description_template, lang, description_data)?,
        input_schema,
        use_input_streaming,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn all_built_in_templates_parse_and_render_with_empty_context() {
        // Every template must (a) parse at startup and (b) render without
        // error against an empty context when it declares no required
        // variable. Templates that DO require a variable are exercised by
        // their own module tests; this loop only guards the static ones.
        // Run against both languages so a parse failure in one is caught.
        let static_templates = [
            PromptTemplate::ModeGoalAddendum,
            PromptTemplate::WrapperMaxTokensDirective,
            PromptTemplate::WrapperUnfulfilledToolIntentNudge,
            PromptTemplate::WrapperToolDenied,
            PromptTemplate::SideCallApprovalSystem,
            PromptTemplate::SideCallGoalSystem,
            PromptTemplate::SideCallCompactSystem,
            PromptTemplate::SideCallCompactFinalInstruction,
            PromptTemplate::TitleFirstInstruction,
        ];
        for lang in [Language::En, Language::ZhCn] {
            for t in static_templates {
                let rendered =
                    render_static(t, lang).expect("template must render with empty context");
                assert!(!rendered.is_empty(), "{t:?} ({lang:?}) rendered empty");
                assert!(
                    !rendered.contains("{{ "),
                    "{t:?} ({lang:?}) left an unsubstituted variable: {rendered}"
                );
            }
        }
    }

    #[test]
    fn every_variant_resolves_to_a_registered_template_in_both_languages() {
        // Touch both globals so `assert_all_registered` runs at init for each.
        let _ = render_static(PromptTemplate::ModeGoalAddendum, Language::En).unwrap();
        let _ = render_static(PromptTemplate::ModeGoalAddendum, Language::ZhCn).unwrap();
        // `assert_all_registered` ties `ALL` to `REGISTRATIONS` (length +
        // per-variant parse) and `name()` is a compile-exhaustive match, so
        // the only thing left to guard is `ALL` itself staying exhaustive
        // over the enum. The count is hand-maintained and must be bumped
        // when a variant is added — this tripwire makes a forgotten bump
        // fail loudly here rather than letting a new variant ship
        // unregistered.
        assert_eq!(template::ALL.len(), 25);
        assert_eq!(REGISTRATIONS.len(), 25);
    }

    /// The on-disk `en/` and `zh-CN/` template trees must carry the same set
    /// of files per subdir — a file added to one language but not the other
    /// is a drift this catches at test time. (Every registered variant is
    /// already compile-checked via `include_str!`; this guards the
    /// unregistered stragglers that `include_str!` cannot see.)
    #[test]
    fn en_and_zh_template_dirs_are_symmetric() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src/prompt/templates");
        for sub in ["mode", "side_call", "system", "title", "tools", "wrapper"] {
            let en = list_md(&format!("{root}/en/{sub}"));
            let zh = list_md(&format!("{root}/zh-CN/{sub}"));
            assert_eq!(
                en, zh,
                "template subdir `{sub}` drifted between en and zh-CN"
            );
        }
    }

    fn list_md(dir: &str) -> HashSet<String> {
        std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("read_dir {dir} failed: {e}"))
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".md"))
            .collect()
    }

    /// Every data-bearing template must fully substitute its variables against
    /// a representative payload, in both languages. Catches a data-struct
    /// field rename that leaves a template variable unsubstituted (which would
    /// otherwise leak `{{ x }}` into a model-facing prompt) at test time
    /// rather than in production.
    #[test]
    fn data_bearing_templates_render_without_leaked_syntax() {
        fn assert_clean(rendered: &str, t: PromptTemplate, lang: Language) {
            assert!(!rendered.is_empty(), "{t:?} ({lang:?}) rendered empty");
            assert!(
                !rendered.contains("{{") && !rendered.contains("{%"),
                "{t:?} ({lang:?}) left unsubstituted template syntax: {rendered}"
            );
        }

        for lang in [Language::En, Language::ZhCn] {
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
                &render(PromptTemplate::SystemMain, lang, &main).unwrap(),
                PromptTemplate::SystemMain,
                lang,
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
            };
            assert_clean(
                &render(PromptTemplate::SystemAssembly, lang, &assembly).unwrap(),
                PromptTemplate::SystemAssembly,
                lang,
            );

            // Conversation wrappers.
            assert_clean(
                &render(
                    PromptTemplate::WrapperMaxTurnsSummary,
                    lang,
                    &crate::prompt::MaxTurnsSummaryData { max: 10 },
                )
                .unwrap(),
                PromptTemplate::WrapperMaxTurnsSummary,
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperInstructionsEager,
                    lang,
                    &crate::prompt::InstructionsPromptData {
                        sources: vec![crate::prompt::InstructionSourcePromptData {
                            scope: "project",
                            path: "/p/CLAUDE.md".to_string(),
                            content: "body".to_string(),
                        }],
                    },
                )
                .unwrap(),
                PromptTemplate::WrapperInstructionsEager,
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperInstructionsLazy,
                    lang,
                    &crate::prompt::InstructionsPromptData {
                        sources: vec![crate::prompt::InstructionSourcePromptData {
                            scope: "project",
                            path: "/p/sub/CLAUDE.md".to_string(),
                            content: "nested body".to_string(),
                        }],
                    },
                )
                .unwrap(),
                PromptTemplate::WrapperInstructionsLazy,
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperRecoveryFailure,
                    lang,
                    &crate::prompt::RecoveryFailureData {
                        reason: "boom".to_string(),
                    },
                )
                .unwrap(),
                PromptTemplate::WrapperRecoveryFailure,
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperEmptyTurnNudge,
                    lang,
                    &crate::prompt::EmptyTurnNudgeData { in_plan: true },
                )
                .unwrap(),
                PromptTemplate::WrapperEmptyTurnNudge,
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperEmptyTurnNudge,
                    lang,
                    &crate::prompt::EmptyTurnNudgeData { in_plan: false },
                )
                .unwrap(),
                PromptTemplate::WrapperEmptyTurnNudge,
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperPeerMessage,
                    lang,
                    &crate::prompt::PeerMessageData {
                        from: "x".to_string(),
                        content: "hi".to_string(),
                    },
                )
                .unwrap(),
                PromptTemplate::WrapperPeerMessage,
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperAskUserQuestions,
                    lang,
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
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperGoalContinuation,
                    lang,
                    &crate::prompt::GoalContinuationData {
                        condition: "c".to_string(),
                    },
                )
                .unwrap(),
                PromptTemplate::WrapperGoalContinuation,
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::WrapperCompactionPreamble,
                    lang,
                    &crate::prompt::CompactionPreambleData {
                        summary: "s".to_string(),
                    },
                )
                .unwrap(),
                PromptTemplate::WrapperCompactionPreamble,
                lang,
            );

            // Side-call user prompts.
            assert_clean(
                &render(
                    PromptTemplate::SideCallApprovalUser,
                    lang,
                    &crate::prompt::ApprovalReviewPromptData {
                        cwd: "/c".to_string(),
                        tool_name: "Bash".to_string(),
                        tool_title: "Bash".to_string(),
                        tool_input: "{}".to_string(),
                    },
                )
                .unwrap(),
                PromptTemplate::SideCallApprovalUser,
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::SideCallGoalUser,
                    lang,
                    &crate::prompt::GoalEvalPromptData {
                        condition: "c".to_string(),
                        last_user: "u".to_string(),
                        last_assistant: "a".to_string(),
                    },
                )
                .unwrap(),
                PromptTemplate::SideCallGoalUser,
                lang,
            );

            // Title topic-shift (uses a sentinel literal in data).
            assert_clean(
                &render(
                    PromptTemplate::TitleTopicShiftInstruction,
                    lang,
                    &crate::prompt::TopicShiftData {
                        current_title: "t".to_string(),
                        unchanged_sentinel: "UNCHANGED",
                    },
                )
                .unwrap(),
                PromptTemplate::TitleTopicShiftInstruction,
                lang,
            );

            // Skill body (both branches: with/without description and arguments).
            assert_clean(
                &render(
                    PromptTemplate::SkillBody,
                    lang,
                    &crate::prompt::SkillBodyData {
                        description: Some("d".to_string()),
                        body: "body".to_string(),
                        arguments: Some("args".to_string()),
                    },
                )
                .unwrap(),
                PromptTemplate::SkillBody,
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::SkillBody,
                    lang,
                    &crate::prompt::SkillBodyData {
                        description: None,
                        body: "body".to_string(),
                        arguments: None,
                    },
                )
                .unwrap(),
                PromptTemplate::SkillBody,
                lang,
            );

            // Agent tool description (both branches: with/without subagents).
            assert_clean(
                &render(
                    PromptTemplate::AgentToolDescription,
                    lang,
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
                lang,
            );
            assert_clean(
                &render(
                    PromptTemplate::AgentToolDescription,
                    lang,
                    &crate::prompt::AgentToolDescriptionData { subagents: vec![] },
                )
                .unwrap(),
                PromptTemplate::AgentToolDescription,
                lang,
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
}
