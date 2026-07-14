//! Strong-typed data payloads for prompt templates.
//!
//! Business code constructs these structs (filling fields only — never
//! pre-formatting markdown) and hands them to [`crate::prompt::render`]. The
//! template owns layout: section headings, conditional rows, list iteration.
//!
//! `Option<T>` → optional block; `Vec<T>` → iterated list. No struct here
//! carries pre-rendered prompt markdown except the main-thread `static_body`,
//! which is plain prose maintained as a sibling `.md` file and embedded at
//! compile time.

use serde::Serialize;

/// A one-line skill summary (`- name: description`) for the system-prompt
/// skills block. Only the summary is advertised; the full body is pulled on
/// demand via the `skill` tool.
#[derive(Debug, Clone, Serialize)]
pub struct SkillSummaryPromptData {
    pub name: String,
    pub description: String,
}

/// Endonym advertised in the one-line language directive ("English" /
/// "Simplified Chinese"). The model parses the directive; the user never sees
/// this string.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct LanguagePromptData {
    pub language: &'static str,
}

/// Runtime identity block. Session-stable rows first (cwd / project / os /
/// shell / python3 / node), then daily-volatile `today`, then
/// toggle-volatile approval mode last — so the cacheable prefix extends as far
/// as possible. `None` approval mode stays silent (the default `OnRequest`
/// case), keeping the identity block byte-stable for the common path.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeIdentityPromptData {
    pub cwd: String,
    pub project: Option<String>,
    pub active_worktree: Option<WorktreePromptData>,
    pub os: &'static str,
    pub shell: String,
    pub python3: String,
    pub node: String,
    pub today: String,
    /// `None` = `OnRequest` (silent). `Some("AutoReview")` / `Some("Yolo")`
    /// advertise the two modes the model can act differently on.
    pub approval_mode: Option<&'static str>,
}

/// A git worktree row in the runtime identity block.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreePromptData {
    pub branch: String,
    pub path: String,
}

/// Main-thread system prompt payload.
#[derive(Debug, Clone, Serialize)]
pub struct MainSystemPromptData {
    /// Plain-markdown static prose, embedded at compile time. Carried as a
    /// variable so the template body stays a thin layout shell and edits to
    /// the prose never touch Rust.
    pub static_body: &'static str,
    pub skills: Vec<SkillSummaryPromptData>,
    pub language: LanguagePromptData,
    pub runtime: RuntimeIdentityPromptData,
}

/// Final system-message assembly at the `build_completion_request` boundary.
///
/// `base` is the pre-rendered base prompt: for the main thread, the rendered
/// [`MainSystemPromptData`]; for a sub-agent, its `agents/*.md` system body.
/// `language` is `Some` only for sub-agents (the main base already bakes the
/// directive in). Mode addendums are toggled by the booleans; the prose lives
/// in the `mode/*.tera.md` templates included by the assembly template.
#[derive(Debug, Clone, Serialize)]
pub struct SystemPromptAssembly {
    pub base: String,
    pub language: Option<LanguagePromptData>,
    pub worktree_subagent: Option<WorktreePromptData>,
    pub goal: bool,
    pub ultracode: bool,
    /// Operator-declared model capability ground truth (provider-config
    /// `supports_tools` / `supports_images`), so the model does not
    /// self-report — and hallucinate — its own capabilities (thread 480b2469:
    /// a non-multimodal model claimed multimodal ability).
    pub capabilities: ModelCapabilitiesPromptData,
}

/// Model capability ground truth injected into the system prompt. Defaults to
/// the common case (tools on, images off) so call sites without a resolved
/// model — tests, restore paths — get the baseline without spelling it out.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ModelCapabilitiesPromptData {
    pub supports_tools: bool,
    pub supports_images: bool,
}

impl Default for ModelCapabilitiesPromptData {
    fn default() -> Self {
        Self {
            supports_tools: true,
            supports_images: false,
        }
    }
}

// --- conversation wrappers ---

#[derive(Debug, Clone, Serialize)]
pub struct MaxTurnsSummaryData {
    pub max: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeerMessageData {
    pub from: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AskUserResponseData {
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AskUserQa {
    pub question: String,
    pub answer: String,
}

/// Multi-question ask-user result: each `{ question, answer }` rendered as a
/// `Question: …\nAnswer: …` block.
#[derive(Debug, Clone, Serialize)]
pub struct AskUserQuestionsData {
    pub answers: Vec<AskUserQa>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecoveryFailureData {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalContinuationData {
    pub condition: String,
}

/// Plan-approval wrapper: echoes the approved plan text back to the model so it
/// can begin execution from the agreed scope.
#[derive(Debug, Clone, Serialize)]
pub struct PlanApprovedData {
    pub plan_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactionPreambleData {
    pub summary: String,
}

// --- side-call prompts ---

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalReviewPromptData {
    pub cwd: String,
    pub tool_name: String,
    pub tool_title: String,
    pub tool_input: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalEvalPromptData {
    pub condition: String,
    pub last_user: String,
    pub last_assistant: String,
}

// --- title ---

#[derive(Debug, Clone, Serialize)]
pub struct TopicShiftData {
    pub current_title: String,
    /// The literal `UNCHANGED` sentinel, so the template does not hardcode it.
    pub unchanged_sentinel: &'static str,
}

// --- command / skill ---

/// Skill turn body: optional description prefix + body + optional args.
#[derive(Debug, Clone, Serialize)]
pub struct SkillBodyData {
    pub description: Option<String>,
    pub body: String,
    pub arguments: Option<String>,
}

// --- tool descriptions ---

/// A sub-agent type advertised in the `agent` tool description.
#[derive(Debug, Clone, Serialize)]
pub struct SubagentTypeData {
    pub name: String,
    /// `read-only` / `write` / `bash` / `write+bash`.
    pub capability: &'static str,
    pub description: String,
}

/// Payload for the `agent` tool description. The static preamble lives in the
/// template; the dynamic sub-agent list is an array so no `push_str` markdown
/// is built in flow code.
#[derive(Debug, Clone, Serialize)]
pub struct AgentToolDescriptionData {
    pub subagents: Vec<SubagentTypeData>,
}
