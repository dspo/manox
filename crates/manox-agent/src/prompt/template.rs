//! Strong-typed prompt template registry.
//!
//! Every model-visible prompt rendered through [`crate::prompt::render`] is
//! addressed by a variant here — business code never writes a bare template
//! path string. Each variant maps to a `.tera.md` file embedded at compile
//! time via [`crate::prompt::renderer`] and addressed by its [`Self::name`]
//! (the Tera registry key). Adding a prompt = add a variant + a template file
//! + register it in [`crate::prompt::renderer::tera`].

/// All built-in, model-visible prompt templates.
///
/// Grouped by concern (system head, mode addendums, conversation wrappers,
/// side-call system/user prompts, title instructions, command/skill wrappers,
/// tool descriptions). The ordering is purely editorial — the render order is
/// decided by the boundary caller, not by this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTemplate {
    // --- system head ---
    /// Main-thread system prompt: static prose + skills + language + runtime
    /// identity. The static prose is carried as a `&'static str` data field so
    /// the template body stays a thin layout shell and the prose remains a
    /// plain-markdown file.
    SystemMain,
    /// Final system message assembly: a pre-rendered base plus optional
    /// sub-agent worktree context. Rendered once at the
    /// `build_completion_request` boundary so no `push_str` lands in flow code.
    SystemAssembly,

    // --- conversation wrappers (inserted into history as user/tool messages) ---
    WrapperMaxTurnsSummary,
    WrapperMaxTokensDirective,
    WrapperRecoveryFailure,
    WrapperEmptyTurnNudge,
    WrapperUnfulfilledToolIntentNudge,
    WrapperDenialBreakerDirective,
    WrapperPeerMessage,
    WrapperAskUserQuestions,
    WrapperToolDenied,

    // --- side-call system + user prompts ---

    // --- title generation ---
    TitleFirstInstruction,
    TitleTopicShiftInstruction,

    // --- command / skill wrappers ---
    SkillBody,

    // --- plan mode ---
    /// Injected every turn while plan mode is active: read-only discipline,
    /// plan-file conventions, research discipline, and the propose contract.
    ModePlanActive,
    /// Execution seed after a plan is approved: read the plan file and
    /// implement it top to bottom.
    ModePlanApproved,

    // --- tool descriptions ---
    AgentToolDescription,
}

impl PromptTemplate {
    /// The Tera registry key (matches the template file path). Stable across
    /// builds; changing a key is a breaking change to the renderer.
    pub const fn name(self) -> &'static str {
        match self {
            Self::SystemMain => "system/main.tera.md",
            Self::SystemAssembly => "system/assembly.tera.md",
            Self::WrapperMaxTurnsSummary => "wrapper/max_turns_summary.tera.md",
            Self::WrapperMaxTokensDirective => "wrapper/max_tokens_directive.tera.md",
            Self::WrapperRecoveryFailure => "wrapper/recovery_failure.tera.md",
            Self::WrapperEmptyTurnNudge => "wrapper/empty_turn_nudge.tera.md",
            Self::WrapperUnfulfilledToolIntentNudge => {
                "wrapper/unfulfilled_tool_intent_nudge.tera.md"
            }
            Self::WrapperDenialBreakerDirective => "wrapper/denial_breaker_directive.tera.md",
            Self::WrapperPeerMessage => "wrapper/peer_message.tera.md",
            Self::WrapperAskUserQuestions => "wrapper/ask_user_questions.tera.md",
            Self::WrapperToolDenied => "wrapper/tool_denied.tera.md",
            Self::TitleFirstInstruction => "title/first.tera.md",
            Self::TitleTopicShiftInstruction => "title/topic_shift.tera.md",
            Self::SkillBody => "wrapper/skill_body.tera.md",
            Self::ModePlanActive => "mode/plan_mode_active.tera.md",
            Self::ModePlanApproved => "mode/plan_mode_approved.tera.md",
            Self::AgentToolDescription => "tools/agent_tool.tera.md",
        }
    }
}

/// Every variant, in editorial order. The single source of truth for "is
/// every variant registered" — the renderer pairs this against its
/// `(variant, source)` table and panics at startup if a variant lacks a
/// template file, rather than deferring the failure to a render-time 500.
pub const ALL: [PromptTemplate; 17] = [
    PromptTemplate::SystemMain,
    PromptTemplate::SystemAssembly,
    PromptTemplate::WrapperMaxTurnsSummary,
    PromptTemplate::WrapperMaxTokensDirective,
    PromptTemplate::WrapperRecoveryFailure,
    PromptTemplate::WrapperEmptyTurnNudge,
    PromptTemplate::WrapperUnfulfilledToolIntentNudge,
    PromptTemplate::WrapperDenialBreakerDirective,
    PromptTemplate::WrapperPeerMessage,
    PromptTemplate::WrapperAskUserQuestions,
    PromptTemplate::WrapperToolDenied,
    PromptTemplate::TitleFirstInstruction,
    PromptTemplate::TitleTopicShiftInstruction,
    PromptTemplate::SkillBody,
    PromptTemplate::ModePlanActive,
    PromptTemplate::ModePlanApproved,
    PromptTemplate::AgentToolDescription,
];
