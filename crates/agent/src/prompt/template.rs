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
    /// Final system message assembly: a pre-rendered base plus optional mode
    /// addendums (goal / ultracode / sub-agent worktree). Rendered once at the
    /// `build_completion_request` boundary so no `push_str` lands in flow code.
    SystemAssembly,

    // --- mode addendums (static prose, no variables) ---
    ModeGoalAddendum,
    ModeUltracodeGrant,

    // --- conversation wrappers (inserted into history as user/tool messages) ---
    WrapperMaxTurnsSummary,
    WrapperMaxTokensDirective,
    WrapperRecoveryFailure,
    WrapperEmptyTurnNudge,
    WrapperUnfulfilledToolIntentNudge,
    WrapperPeerMessage,
    WrapperAskUserResponse,
    WrapperAskUserQuestions,
    WrapperToolDenied,
    WrapperGoalContinuation,
    WrapperCompactionPreamble,

    // --- side-call system + user prompts ---
    SideCallApprovalSystem,
    SideCallApprovalUser,
    SideCallGoalSystem,
    SideCallGoalUser,
    SideCallCompactSystem,
    SideCallCompactFinalInstruction,

    // --- title generation ---
    TitleFirstInstruction,
    TitleTopicShiftInstruction,

    // --- command / skill wrappers ---
    SkillBody,

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
            Self::ModeGoalAddendum => "mode/goal.tera.md",
            Self::ModeUltracodeGrant => "mode/ultracode.tera.md",
            Self::WrapperMaxTurnsSummary => "wrapper/max_turns_summary.tera.md",
            Self::WrapperMaxTokensDirective => "wrapper/max_tokens_directive.tera.md",
            Self::WrapperRecoveryFailure => "wrapper/recovery_failure.tera.md",
            Self::WrapperEmptyTurnNudge => "wrapper/empty_turn_nudge.tera.md",
            Self::WrapperUnfulfilledToolIntentNudge => {
                "wrapper/unfulfilled_tool_intent_nudge.tera.md"
            }
            Self::WrapperPeerMessage => "wrapper/peer_message.tera.md",
            Self::WrapperAskUserResponse => "wrapper/ask_user_response.tera.md",
            Self::WrapperAskUserQuestions => "wrapper/ask_user_questions.tera.md",
            Self::WrapperToolDenied => "wrapper/tool_denied.tera.md",
            Self::WrapperGoalContinuation => "wrapper/goal_continuation.tera.md",
            Self::WrapperCompactionPreamble => "wrapper/compaction_preamble.tera.md",
            Self::SideCallApprovalSystem => "side_call/approval_system.tera.md",
            Self::SideCallApprovalUser => "side_call/approval_user.tera.md",
            Self::SideCallGoalSystem => "side_call/goal_system.tera.md",
            Self::SideCallGoalUser => "side_call/goal_user.tera.md",
            Self::SideCallCompactSystem => "side_call/compact_system.tera.md",
            Self::SideCallCompactFinalInstruction => "side_call/compact_final.tera.md",
            Self::TitleFirstInstruction => "title/first.tera.md",
            Self::TitleTopicShiftInstruction => "title/topic_shift.tera.md",
            Self::SkillBody => "wrapper/skill_body.tera.md",
            Self::AgentToolDescription => "tools/agent_tool.tera.md",
        }
    }
}

/// Every variant, in editorial order. The single source of truth for "is
/// every variant registered" — the renderer pairs this against its
/// `(variant, source)` table and panics at startup if a variant lacks a
/// template file, rather than deferring the failure to a render-time 500.
pub const ALL: [PromptTemplate; 25] = [
    PromptTemplate::SystemMain,
    PromptTemplate::SystemAssembly,
    PromptTemplate::ModeGoalAddendum,
    PromptTemplate::ModeUltracodeGrant,
    PromptTemplate::WrapperMaxTurnsSummary,
    PromptTemplate::WrapperMaxTokensDirective,
    PromptTemplate::WrapperRecoveryFailure,
    PromptTemplate::WrapperEmptyTurnNudge,
    PromptTemplate::WrapperUnfulfilledToolIntentNudge,
    PromptTemplate::WrapperPeerMessage,
    PromptTemplate::WrapperAskUserResponse,
    PromptTemplate::WrapperAskUserQuestions,
    PromptTemplate::WrapperToolDenied,
    PromptTemplate::WrapperGoalContinuation,
    PromptTemplate::WrapperCompactionPreamble,
    PromptTemplate::SideCallApprovalSystem,
    PromptTemplate::SideCallApprovalUser,
    PromptTemplate::SideCallGoalSystem,
    PromptTemplate::SideCallGoalUser,
    PromptTemplate::SideCallCompactSystem,
    PromptTemplate::SideCallCompactFinalInstruction,
    PromptTemplate::TitleFirstInstruction,
    PromptTemplate::TitleTopicShiftInstruction,
    PromptTemplate::SkillBody,
    PromptTemplate::AgentToolDescription,
];
