//! Kernel session records → AHP actions.
//!
//! The kernel's jsonl session store is the only durable authority (§C, L3/L4);
//! this module is the single projection from its [`SessionTreeEntry`] records
//! to the AHP actions that carry the same facts, paired with the channel they
//! belong on. There is no intermediate wire vocabulary: the record's own
//! fields drive every arm, so a new kernel variant fails to compile here —
//! the moment the AHP mapping must state where it belongs.
//!
//! # Shape of the mapping
//!
//! - **Transcript rows** (`Message` / `UiNote` / `Custom` / `CustomMessage`)
//!   become ordered response parts. A `user` row rides the next
//!   `chat/turnStarted` — `_meta["x-manox"].originRpc` is what retires the
//!   client's optimistic echo (L7) — and is visible as a queued pending
//!   message until then, so no durable row is ever invisible.
//! - **Streaming** follows AHP's create-then-append contract: the first delta
//!   of a run emits `chat/responsePart` creating an empty markdown / reasoning
//!   part, later deltas append with `chat/delta` / `chat/reasoning`. A run is
//!   *contiguous* text (or thinking): the other stream, a tool call, a
//!   notification or a turn boundary closes it, so the next delta opens a new
//!   part. That state machine is why this type exists.
//! - **Tool calls** map the kernel's status strings onto AHP's lifecycle
//!   ([`status_phase`]); approvals drive the `pending-confirmation ⇄ running /
//!   cancelled` transitions. Fail-closed approval policy is the runtime's —
//!   this is projection only.
//! - **What AHP cannot represent** goes to the x-manox surface declared in
//!   [`manox_ahp::ext`] (extension channels for plan / work / metrics,
//!   extension actions on standard channels for session facts with no field —
//!   pin, label, leaf cursor), or, when the fact belongs in the transcript's
//!   order, to a `systemNotification` response part. Each arm names which
//!   route it takes.
//!
//! # Determinism
//!
//! Every identity minted here derives from a kernel entry id: turns
//! (`t-<turnStart id>`), parts (`p-<first delta of the run id>`), pending
//! messages (`m-<row id>`). Replaying the same records yields the same ids in
//! the same order — L10's `snapshot == fold(replay)`, and why a restarted host
//! can seed from one fold and keep advancing it.
//!
//! # Duplicated facts
//!
//! Streaming deltas *and* the settled assistant row both carry the reply text;
//! `ToolCall{success}` and the following `ToolResult` both end a call. The rule
//! is first-writer-wins, the other silent, so the fold holds each fact once.
//! The arms say which.

use std::collections::{BTreeMap, VecDeque};

use ahp_types::actions::{
    ChatActivityChangedAction, ChatDeltaAction, ChatErrorAction, ChatInputCompletedAction,
    ChatInputRequestedAction, ChatPendingMessageSetAction, ChatReasoningAction,
    ChatResponsePartAction, ChatToolCallCompleteAction, ChatToolCallConfirmedAction,
    ChatToolCallContentChangedAction, ChatToolCallDeltaAction, ChatToolCallReadyAction,
    ChatToolCallStartAction, ChatTurnCancelledAction, ChatTurnCompleteAction,
    ChatTurnStartedAction, ChatUsageAction, PartialChatSummary, SessionActivityChangedAction,
    SessionChatUpdatedAction, SessionConfigChangedAction, SessionInputNeededRemovedAction,
    SessionInputNeededSetAction, SessionIsArchivedChangedAction, SessionTitleChangedAction,
    SessionWorkingDirectorySetAction, StateAction,
};
use ahp_types::common::{JsonObject, StringOrMarkdown, Uri};
use ahp_types::state::{
    ChatInputMultiSelectQuestion, ChatInputOption, ChatInputQuestion, ChatInputRequest,
    ChatInputResponseKind, ChatInputSingleSelectQuestion, ChatInputTextQuestion, ChatState,
    ConfirmationOption, ConfirmationOptionKind, ErrorInfo, ErrorResponsePart, MarkdownResponsePart,
    Message, MessageAttachment, MessageEmbeddedResourceAttachment, MessageKind, MessageOrigin,
    PendingMessageKind, ReasoningResponsePart, ResponsePart, SessionInputRequest,
    SessionToolConfirmationRequest, SystemNotificationResponsePart, ToolCallCancellationReason,
    ToolCallConfirmationReason, ToolCallConfirmationState, ToolCallPendingConfirmationState,
    ToolCallResult, ToolCallState, ToolResultContent, ToolResultTextContent, UsageInfo,
};
use manox_harness::session::{SessionTreeEntry, plan_review_request_id};
use manox_harness::types::{AgentMessage, ContentBlock};
use serde_json::{Value, json};

use manox_ahp::ext;

/// Session config value keys this translator writes.
///
/// The runtime's `ConfigSchema` declares the same keys: this is the naming
/// authority for the values that ride `session/configChanged` — the spec's config
/// model (model / effort / approval mode) plus the session facts AHP has no field
/// for.
pub mod config_keys {
    /// Canonical `provider/model` reference (L8).
    pub const MODEL: &str = "model";
    /// Reasoning-effort vocabulary string.
    pub const REASONING_EFFORT: &str = "reasoningEffort";
    /// Permission / approval-mode vocabulary string.
    pub const APPROVAL_MODE: &str = "approvalMode";
    /// Server-owned project path (`null` clears).
    pub const PROJECT: &str = "project";
    /// Effective working directory of this journal.
    pub const WORKING_DIRECTORY: &str = "workingDirectory";
}

/// The journal-row facts the projection mints identities from: one kernel
/// record's chain depth, entry id, and append timestamp. Built per record by
/// [`Translator::on_entry`]; the action builders take it so their bodies stay
/// about the protocol, not the record's column list.
pub(crate) struct Envelope {
    pub seq: u64,
    pub id: String,
    pub timestamp: String,
}

/// One assistant row's token usage, decoded from the kernel's usage stats.
pub(crate) struct UsageRow {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

/// Kernel content blocks serialize through the wire-opaque storage shape
/// (§C.2); an unspecifiable block is dropped rather than failing the frame.
fn content_blocks(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|b| {
            serde_json::to_value(b)
                .map_err(|e| {
                    tracing::warn!(error = %e, "content block dropped (serialization failure)");
                })
                .ok()
        })
        .collect()
}

/// The full kernel JSON object of a non-transcript message (wire-opaque).
fn message_value(message: &AgentMessage) -> Vec<Value> {
    match serde_json::to_value(message) {
        Ok(v) => vec![v],
        Err(e) => {
            tracing::error!(error = %e, "message serialization failed");
            vec![serde_json::json!({
                "type": "error",
                "text": format!("message serialization failed: {e}"),
            })]
        }
    }
}

/// The channel family a kernel record publishes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// `ahp-chat:/<id>` — transcript rows, streaming, tool calls, approvals.
    Chat,
    /// `ahp-session:/<id>` — title, working directories, config, catalog.
    Session,
    /// `x-manox-plan:/<chat-id>` — plan mode, plan document.
    Plan,
    /// `x-manox-work:/<session-id>` — goal, background work, sub-agents, suites.
    Work,
    /// `x-manox-metrics:/<chat-id>` — aggregated conversation metrics.
    Metrics,
    /// `x-manox-thread:/<session-id>` — pin, label, session info, leaf cursor.
    ///
    /// These rows have no AHP-native slot and only fold on an extension
    /// channel: emitted on `ahp-session:/<id>` they are `StateAction::Unknown`
    /// to the session reducer, land in the fold's session bucket, and are
    /// unreachable from the extension state and its baselines.
    Thread,
}

impl Target {
    /// The channel URI this target resolves to, given the owning chat/session.
    fn uri(self, chat_id: &str, session_id: &str) -> String {
        match self {
            Self::Chat => manox_ahp::channels::chat::uri(chat_id),
            Self::Session => manox_ahp::channels::session::uri(session_id),
            Self::Plan => format!("{}{chat_id}", ext::channels::PLAN),
            Self::Work => format!("{}{session_id}", ext::channels::WORK),
            Self::Metrics => format!("{}{chat_id}", ext::channels::METRICS),
            Self::Thread => format!("{}{session_id}", ext::channels::THREAD),
        }
    }
}

/// Which channel a kernel record lands on — the compile-time totality gate:
/// a kernel variant added without deciding where it lands is a compile error.
pub(crate) fn target_of(entry: &SessionTreeEntry) -> Target {
    use SessionTreeEntry as E;
    match entry {
        // ── transcript ───────────────────────────────────────────────
        E::Message { .. } | E::UiNote { .. } | E::Custom { .. } | E::CustomMessage { .. } => {
            Target::Chat
        }
        // ── lifecycle ────────────────────────────────────────────────
        E::TurnStart { .. } | E::TurnFinish { .. } | E::Stop { .. } | E::Retry { .. } => {
            Target::Chat
        }
        E::ErrorEvent { .. } => Target::Chat,
        // ── streaming / tool activity ────────────────────────────────
        E::AgentTextDelta { .. }
        | E::AgentThinkingDelta { .. }
        | E::ToolCall { .. }
        | E::ToolResult { .. }
        | E::ToolOutputChunk { .. }
        | E::SubagentChild { .. } => Target::Chat,
        E::SubagentProgress { .. } => Target::Work,
        // ── session state ────────────────────────────────────────────
        E::ModelChange { .. }
        | E::CwdChange { .. }
        | E::ProjectChange { .. }
        | E::PermissionModeChange { .. }
        | E::ThinkingLevelChange { .. }
        | E::Title { .. }
        // `PinnedArchived` lands here for its native archived half; the
        // extension pin half re-targets the thread channel at its emit site.
        | E::PinnedArchived { .. } => Target::Session,
        // Thread rows: the extension face is their only face — on the session
        // channel no fold ever consumed them (see [`Target::Thread`]).
        E::Label { .. } | E::SessionInfo { .. } | E::Leaf { .. } => Target::Thread,
        // ── plan ─────────────────────────────────────────────────────
        E::PlanModeChange { .. } | E::PlanModeRequest { .. } | E::PlanUpdate { .. } => Target::Plan,
        // ── extension work surface ───────────────────────────────────
        E::Goal { .. } | E::BrowserSuites { .. } | E::BackgroundTask { .. } => Target::Work,
        E::ActiveToolsChange { .. } => Target::Work,
        // ── approvals / questions / plan review ride the chat's tool-call +
        //    input state (the plan-review card is a `chat/inputRequested`) ──
        E::Approval { .. } | E::Question { .. } | E::PlanReview { .. } => Target::Chat,
        // ── compaction and branch summaries surface in the transcript ──
        E::Compaction { .. } | E::CompactionStarted { .. } | E::BranchSummary { .. } => {
            Target::Chat
        }
        // ── metrics ──────────────────────────────────────────────────
        E::Metrics { .. } => Target::Metrics,
    }
}

/// One action plus the channel it must be published on.
#[derive(Debug, Clone, PartialEq)]
pub struct Emitted {
    /// Target channel URI.
    pub channel: Uri,
    /// The action to publish.
    pub action: StateAction,
}

impl Emitted {
    fn new(channel: &str, action: StateAction) -> Self {
        Self {
            channel: channel.to_string(),
            action,
        }
    }

    /// The action's wire tag, for logs and the declaration-surface gates.
    ///
    /// Derived rather than stored: the tag *is* what serialization emits, so a
    /// stored copy could only drift from the action it labels.
    pub fn tag(&self) -> String {
        manox_ahp::wire::action_tag(&self.action)
    }
}

/// Mirror a chat activity edge onto the session channel, per the spec's
/// "producers SHOULD also update the parent session's chat catalog" guidance:
/// the session-level activity string follows the active chat's, and the chat's
/// catalogue entry rides `session/chatUpdated`.
///
/// A cleared edge (`None`) cannot clear the catalogue entry —
/// `PartialChatSummary.activity` is `None`-means-*unchanged*, not *cleared* —
/// so the catalogue keeps its last description until the next set edge, while
/// the session-level string (whose reducer treats `None` as *clear*) does
/// reset.
///
/// Assumes one active chat per session: the mirror writes any chat's edge
/// straight onto the session-level field, so two concurrently active chats
/// in one session would overwrite each other. True today (manox runs one
/// journal per session, and compaction is the only producer); revisit if a
/// second activity producer appears.
fn mirror_activity(chat: &str, session: &str, activity: Option<String>, out: &mut Vec<Emitted>) {
    out.push(Emitted::new(
        session,
        StateAction::SessionActivityChanged(SessionActivityChangedAction {
            activity: activity.clone(),
        }),
    ));
    if let Some(activity) = activity {
        out.push(Emitted::new(
            session,
            StateAction::SessionChatUpdated(SessionChatUpdatedAction {
                chat: chat.to_string(),
                changes: PartialChatSummary {
                    activity: Some(activity),
                    ..PartialChatSummary::default()
                },
            }),
        ));
    }
}

/// How far a journal tool-call status has advanced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Announced, not executing (`pending`, `pending-approval`).
    Announced,
    /// Executing (`running`, `continued`).
    Working,
    /// Execution ended, reported successful (`done`, `success`).
    Finished,
    /// Execution ended badly (`error`).
    Failed,
    /// The call will not run (`denied`, `cancelled`).
    Aborted,
}

/// The journal's tool-call status strings onto [`Phase`].
///
/// Two vocabularies meet: §C.2 documents `pending|running|done|error`, the engine
/// journals the kebab-case names of its own status enum
/// (`pending-approval|running|success|continued|error|denied|cancelled`). The wire
/// field is a free string, so both are accepted, and an unrecognised value becomes
/// [`Phase::Announced`] — the weakest claim, never a false one.
pub fn status_phase(status: &str) -> Phase {
    match status {
        "running" | "continued" => Phase::Working,
        "done" | "success" => Phase::Finished,
        "error" => Phase::Failed,
        "denied" | "cancelled" => Phase::Aborted,
        _ => Phase::Announced,
    }
}

/// One tool call's lifecycle as the wire has seen it.
#[derive(Debug, Clone)]
struct CallBook {
    tool_name: String,
    title: String,
    /// A `chat/toolCallStart` created the part.
    started: bool,
    /// The part sits in `pending-confirmation`.
    awaiting: bool,
    /// The part is `running`.
    working: bool,
    /// A terminal action settled the part (`completed` or `cancelled`).
    settled: bool,
    /// Live output accumulated for `chat/toolCallContentChanged`, which replaces
    /// the content array rather than appending to it.
    output: String,
}

impl CallBook {
    fn new(tool_name: &str, title: &str) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            title: title.to_string(),
            started: false,
            awaiting: false,
            working: false,
            settled: false,
            output: String::new(),
        }
    }

    /// Adopt the state a seeded (already folded) tool call shows.
    fn adopt(title: &str, state: &ToolCallState) -> Self {
        let mut book = Self::new("", title);
        book.started = true;
        match state {
            ToolCallState::Streaming(_) => {}
            ToolCallState::PendingConfirmation(_) => book.awaiting = true,
            ToolCallState::Running(_) | ToolCallState::AuthRequired(_) => book.working = true,
            ToolCallState::PendingResultConfirmation(_) => book.working = true,
            ToolCallState::Completed(_) | ToolCallState::Cancelled(_) => {
                book.working = true;
                book.settled = true;
            }
            ToolCallState::Unknown(_) => {}
        }
        book
    }
}

/// The turn currently open on the chat, with its streaming runs.
#[derive(Debug, Clone)]
struct OpenTurn {
    id: String,
    started_at: String,
    /// Minted by this translator rather than by a `turnStart` row: a transcript
    /// row arrived with no turn to hang on.
    synthetic: bool,
    text_part: Option<String>,
    reasoning_part: Option<String>,
    /// A text delta streamed since the last settled assistant row.
    streamed_text: bool,
    /// The opening user message the turn currently carries: empty until a
    /// user row lands (journal order puts `turnStart` before the user row).
    message_text: String,
    /// Tool calls by handle, in a sorted map so turn-boundary settlement is
    /// deterministic.
    calls: BTreeMap<String, CallBook>,
}

impl OpenTurn {
    fn new(id: String, started_at: String, synthetic: bool) -> Self {
        Self {
            id,
            started_at,
            synthetic,
            message_text: String::new(),
            text_part: None,
            reasoning_part: None,
            streamed_text: false,
            calls: BTreeMap::new(),
        }
    }

    fn close_runs(&mut self) {
        self.text_part = None;
        self.reasoning_part = None;
    }
}

/// A durable user row waiting for the turn that carries it.
#[derive(Debug, Clone)]
struct PendingUser {
    /// The `chat/pendingMessageSet` id already published for it.
    id: String,
    message: Message,
}

/// How a turn ended, per the row that closed it.
#[derive(Debug, Clone)]
enum Outcome {
    Complete,
    Cancelled,
    Failed(ErrorInfo),
}

impl Outcome {
    fn tag(&self) -> &'static str {
        match self {
            Outcome::Complete => "complete",
            Outcome::Cancelled => "cancelled",
            Outcome::Failed(_) => "failed",
        }
    }
}

/// Which streaming run a delta belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    Text,
    Thinking,
}

/// Stateful journal → AHP action translator.
///
/// One instance drives one chat (a journal): it holds the open turn, the open
/// streaming runs and per-call bookkeeping. Drive it entry by entry in journal
/// order and publish the emitted actions in that order (the single stamp point is
/// the host's, §C L4).
#[derive(Debug, Default)]
pub struct Translator {
    open: Option<OpenTurn>,
    pending_user: VecDeque<PendingUser>,
    /// Directories already granted, so repeated `cwdChange` rows for the same path
    /// do not re-publish membership.
    granted: Vec<Uri>,
    /// The open plan-review card's request id, so the `resolved` edge (which
    /// carries no proposal id) can close the very part the proposal opened.
    plan_review: Option<String>,
}

/// One transcript row's decoded fields.
///
/// Grouped rather than passed positionally: the row vocabulary is the kernel's,
/// and a struct keeps the action builder's signature about its job (build the
/// actions for one row) instead of about the kernel's column list.
struct MessageRow<'a> {
    role: &'a str,
    content: &'a [Value],
    usage: Option<&'a UsageRow>,
    origin_rpc: Option<&'a str>,
    display: Option<bool>,
}

/// One transcript row decoded from the kernel's [`AgentMessage`] shape —
/// the wire-opaque content blocks serialized once, the usage flattened, the
/// kernel-extension display flag surfaced.
struct DecodedMessage {
    role: String,
    content: Vec<Value>,
    usage: Option<UsageRow>,
    origin_rpc: Option<String>,
    display: Option<bool>,
}

fn decode_message(message: &AgentMessage, origin_rpc: Option<&str>) -> DecodedMessage {
    match message {
        AgentMessage::User { content, .. } => DecodedMessage {
            role: "user".to_string(),
            content: content_blocks(content),
            usage: None,
            origin_rpc: origin_rpc.map(str::to_string),
            display: None,
        },
        AgentMessage::Assistant { content, usage, .. } => DecodedMessage {
            role: "assistant".to_string(),
            content: content_blocks(content),
            usage: Some(UsageRow {
                input: usage.input_tokens,
                output: usage.output_tokens,
                cache_read: usage.cache_read_input_tokens,
                cache_write: usage.cache_creation_input_tokens,
                reasoning: usage.reasoning_tokens.unwrap_or(0),
            }),
            origin_rpc: None,
            display: None,
        },
        AgentMessage::ToolResult { .. } | AgentMessage::BashExecution { .. } => DecodedMessage {
            role: "tool".to_string(),
            content: message_value(message),
            usage: None,
            origin_rpc: None,
            display: None,
        },
        // Kernel-extension message roles ride the generic transcript row with
        // the kernel JSON shape verbatim (wire-opaque); the kernel's
        // UI-visibility flag rides along so clients can filter hidden rows
        // (the model context is unaffected — the row is here either way).
        AgentMessage::Custom { display, .. } => DecodedMessage {
            role: "custom".to_string(),
            content: message_value(message),
            usage: None,
            origin_rpc: None,
            display: Some(*display),
        },
    }
}

/// One elicitation row's decoded fields (see [`MessageRow`]).
struct AskRow<'a> {
    kind: &'a str,
    auth_id: &'a str,
    tool_name: Option<&'a str>,
    verdict: Option<&'a str>,
    reason: Option<&'a str>,
    input: Option<&'a serde_json::Value>,
}

/// Map one AskUserQuestion's durable `{questions: [...]}` input onto AHP's
/// elicitation question list. A question with options becomes single/multi
/// select (free text still allowed); an option-less question is free text.
fn map_ask_questions(input: &serde_json::Value) -> Option<Vec<ChatInputQuestion>> {
    let questions = input.get("questions")?.as_array()?;
    let mut out = Vec::with_capacity(questions.len());
    for (idx, q) in questions.iter().enumerate() {
        // Journals written by older engine builds state the question in
        // `header` alone (the `question` field is a later schema addition;
        // device journals carry both shapes), so the text falls back to it.
        // A row missing BOTH has no question to ask: it is skipped, and a
        // list that empties this way degrades to the accepted bare ask
        // (`questions: None` below) rather than a silent textless card.
        let message = match q
            .get("question")
            .and_then(|v| v.as_str())
            .or_else(|| q.get("header").and_then(|v| v.as_str()))
        {
            Some(text) => text.to_string(),
            None => continue,
        };
        let title = q.get("header").and_then(|v| v.as_str()).map(str::to_string);
        let id = q
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{idx}"));
        let multi = q
            .get("multiSelect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let options = q.get("options").and_then(|v| v.as_array()).map(|opts| {
            opts.iter()
                .filter_map(|o| {
                    let label = o.get("label")?.as_str()?.to_string();
                    Some(ChatInputOption {
                        id: label.clone(),
                        label: label.clone(),
                        description: o
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        recommended: o.get("recommended").and_then(|v| v.as_bool()),
                    })
                })
                .collect::<Vec<_>>()
        });
        let question = match options {
            Some(opts) if multi => ChatInputQuestion::MultiSelect(ChatInputMultiSelectQuestion {
                id,
                title,
                message,
                required: None,
                options: opts,
                allow_freeform_input: Some(true),
                min: None,
                max: None,
            }),
            Some(opts) => ChatInputQuestion::SingleSelect(ChatInputSingleSelectQuestion {
                id,
                title,
                message,
                required: None,
                options: opts,
                allow_freeform_input: Some(true),
            }),
            None => ChatInputQuestion::Text(ChatInputTextQuestion {
                id,
                title,
                message,
                required: None,
                format: None,
                min: None,
                max: None,
                default_value: None,
            }),
        };
        out.push(question);
    }
    // A list that emptied (every row skipped for having no question text)
    // degrades to the accepted bare ask, not to an empty question card.
    if out.is_empty() {
        return None;
    }
    Some(out)
}

impl Translator {
    /// A translator with no open turn.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt an already-folded transcript as the starting view.
    ///
    /// Used when the host starts against an existing journal: the durable fold has
    /// already produced the chat state, and streaming must continue *into* it. A
    /// run is open exactly when the active turn's last response part is markdown
    /// or reasoning — the invariant this translator maintains — and each tool call
    /// adopts the status the fold left it in.
    pub fn seed(&mut self, chat: &ChatState) {
        self.open = None;
        self.pending_user.clear();
        self.granted.clear();
        self.plan_review = None;
        let Some(active) = &chat.active_turn else {
            return;
        };
        let mut turn = OpenTurn::new(active.id.clone(), active.started_at.clone(), false);
        match active.response_parts.last() {
            Some(ResponsePart::Markdown(part)) => turn.text_part = Some(part.id.clone()),
            Some(ResponsePart::Reasoning(part)) => turn.reasoning_part = Some(part.id.clone()),
            _ => {}
        }
        for part in &active.response_parts {
            if let ResponsePart::ToolCall(tool) = part {
                let id = tool_call_handle(&tool.tool_call).to_string();
                let title = tool_call_intention(&tool.tool_call).unwrap_or_default();
                turn.calls
                    .insert(id, CallBook::adopt(&title, &tool.tool_call));
            }
        }
        self.open = Some(turn);
    }

    /// Project one accepted kernel record into the actions it produces.
    ///
    /// An empty result is a decision, not an oversight, and each case is named
    /// where it arises: a `tool` transcript row whose `ToolResult` already
    /// settled the call, a verdict for a turn that closed, an idle `stop` with
    /// nothing running. The load-bearing semantics are gated by tests below:
    /// `fold_semantics_tests` (steer vs queued, stop as metadata, ask
    /// degradation) and `extension_gate_tests` (every extension row lands on
    /// the channel its fold lives on).
    pub fn on_entry(
        &mut self,
        chat_id: &str,
        session_id: &str,
        seq: u64,
        entry: &SessionTreeEntry,
    ) -> Vec<Emitted> {
        let row = Envelope {
            seq,
            id: entry.id().to_string(),
            timestamp: entry
                .timestamp()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        let at = target_of(entry).uri(chat_id, session_id);
        let chat = Target::Chat.uri(chat_id, session_id);
        let session = Target::Session.uri(chat_id, session_id);
        let thread = Target::Thread.uri(chat_id, session_id);
        let plan = Target::Plan.uri(chat_id, session_id);
        let mut out = Vec::new();
        match entry {
            // ── transcript ───────────────────────────────────────────────
            SessionTreeEntry::Message {
                message, origin, ..
            } => {
                let decoded = decode_message(message, origin.as_deref());
                self.on_message(
                    &chat,
                    &row,
                    MessageRow {
                        role: decoded.role.as_str(),
                        content: &decoded.content,
                        usage: decoded.usage.as_ref(),
                        origin_rpc: decoded.origin_rpc.as_deref(),
                        display: decoded.display,
                    },
                    &mut out,
                );
            }
            SessionTreeEntry::UiNote { note, .. } => {
                let kind = note
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("notice")
                    .to_string();
                let text = note_text(note).unwrap_or_else(|| kind.clone());
                self.push_note(
                    &chat,
                    &row,
                    text,
                    json!({"uiNote": {"kind": kind, "data": note}}),
                    &mut out,
                );
            }
            SessionTreeEntry::Custom {
                custom_type, data, ..
            } => {
                let data = data.clone().unwrap_or(Value::Null);
                let text = note_text(&data).unwrap_or_else(|| custom_type.clone());
                self.push_note(
                    &chat,
                    &row,
                    text,
                    json!({"custom": {"customType": custom_type, "data": data}}),
                    &mut out,
                );
            }
            SessionTreeEntry::CustomMessage {
                custom_type,
                content,
                display,
                ..
            } => {
                let blocks = content_blocks(content);
                let text = blocks_text(&blocks);
                let fallback = custom_type.clone();
                let body = if text.is_empty() { fallback } else { text };
                self.push_note(
                    &chat,
                    &row,
                    body,
                    json!({"customMessage": {
                        "customType": custom_type,
                        "display": display,
                        "blocks": blocks,
                    }}),
                    &mut out,
                );
            }
            // ── turn lifecycle ───────────────────────────────────────────
            SessionTreeEntry::TurnStart { .. } => self.on_turn_start(&chat, &row, &mut out),
            SessionTreeEntry::TurnFinish {
                cancelled,
                failed,
                stranded_steer_ids,
                ..
            } => {
                let outcome = if *failed {
                    Outcome::Failed(ErrorInfo {
                        error_type: "turn_failed".to_string(),
                        message: "the model turn failed".to_string(),
                        stack: None,
                        // The stranded steer ids are the engine's own message ids,
                        // not the pending-message ids published here, so they ride
                        // `_meta` for forensics instead of driving a removal that
                        // could never match.
                        meta: Some(manox_meta(json!({
                            "cancelled": cancelled,
                            "strandedSteerIds": stranded_steer_ids,
                        }))),
                    })
                } else if *cancelled {
                    Outcome::Cancelled
                } else {
                    Outcome::Complete
                };
                self.close_turn(&chat, &row, outcome, &mut out);
            }
            // A `stop` row is per-assistant-message metadata — the model's own
            // stop reason (end_turn / tool_use / max_tokens / refusal /
            // cancelled) — never turn lifecycle: `tool_use` and `max_tokens`
            // are the loop's continue edges (the tool / continuation rows
            // follow them), and a turn end always carries its `TurnFinish`
            // right behind the last stop. Closing the turn here split every
            // agentic turn per tool round and minted a "stopped: …" notice
            // per round.
            SessionTreeEntry::Stop { .. } => {}
            SessionTreeEntry::ErrorEvent { message, .. } => self.close_turn(
                &chat,
                &row,
                Outcome::Failed(ErrorInfo {
                    error_type: "agent_error".to_string(),
                    message: message.clone(),
                    stack: None,
                    meta: None,
                }),
                &mut out,
            ),
            SessionTreeEntry::Retry {
                attempt,
                max_attempts,
                delay_secs,
                reason,
                detail,
                ..
            } => {
                // AHP has no retry concept: a provider retry is harness chatter
                // inside a live turn, so it surfaces as a notification part in the
                // transcript's order, with the counters in `_meta`. The kernel's
                // diagnostic `detail` folds into the reason; the note carries one
                // string.
                let reason = match detail {
                    Some(d) if !d.is_empty() => format!("{reason}: {d}"),
                    _ => reason.clone(),
                };
                self.push_note(
                    &chat,
                    &row,
                    format!("retrying ({attempt}/{max_attempts}) in {delay_secs}s: {reason}"),
                    json!({"retry": {
                        "attempt": attempt,
                        "maxAttempts": max_attempts,
                        "delaySecs": delay_secs,
                        "reason": reason,
                    }}),
                    &mut out,
                );
            }
            // ── streaming ────────────────────────────────────────────────
            SessionTreeEntry::AgentTextDelta { delta, .. } => {
                self.on_delta(&chat, &row, Run::Text, delta, &mut out);
            }
            SessionTreeEntry::AgentThinkingDelta { delta, .. } => {
                self.on_delta(&chat, &row, Run::Thinking, delta, &mut out);
            }
            // ── tool activity ────────────────────────────────────────────
            SessionTreeEntry::ToolCall {
                call_id,
                name,
                title,
                status,
                input,
                ..
            } => {
                let input = input.clone().unwrap_or(Value::Null);
                self.on_tool_call(&chat, &row, call_id, name, title, status, &input, &mut out);
            }
            SessionTreeEntry::ToolResult {
                call_id,
                output,
                is_error,
                ..
            } => self.on_tool_result(&chat, &row, call_id, output, *is_error, &mut out),
            SessionTreeEntry::ToolOutputChunk { call_id, chunk, .. } => {
                self.on_tool_output(&chat, &row, call_id, chunk, &mut out);
            }
            // ── adjudications ────────────────────────────────────────────
            SessionTreeEntry::Approval {
                kind,
                auth_id,
                payload,
                ..
            } => {
                let field = |key: &str| payload.get(key).and_then(|v| v.as_str());
                self.on_approval(
                    &chat,
                    &session,
                    &row,
                    kind,
                    auth_id,
                    field("toolName"),
                    field("toolCallId"),
                    field("verdict"),
                    field("reason"),
                    &mut out,
                );
            }
            SessionTreeEntry::Question {
                kind,
                auth_id,
                payload,
                ..
            } => {
                let field = |key: &str| payload.get(key).and_then(|v| v.as_str());
                self.on_question(
                    &chat,
                    &row,
                    AskRow {
                        kind,
                        auth_id,
                        tool_name: field("toolName"),
                        verdict: field("verdict"),
                        reason: field("reason"),
                        input: payload.get("input"),
                    },
                    &mut out,
                );
            }
            // ── sub-agent activity ───────────────────────────────────────
            SessionTreeEntry::SubagentChild { .. } => {
                // One child session's lifecycle event, in the parent
                // transcript's order — and nothing to push for it: the
                // sub-agent tree is `x-manox-work` state (the Progress rows
                // below maintain it), and a notice per event flooded the
                // transcript with a card per spawn/exit the moment the bridge
                // streamed again.
            }
            SessionTreeEntry::SubagentProgress {
                agent_id,
                agent_type,
                tool_uses,
                latest_activity,
                status,
                ..
            } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::WORK_SUBAGENTS,
                    json!({"agentId": agent_id, "agentType": agent_type,
                           "toolUses": tool_uses, "latestActivity": latest_activity,
                           "status": status}),
                ),
            )),
            SessionTreeEntry::ActiveToolsChange {
                active_tool_names, ..
            } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::WORK_ACTIVE_TOOLS,
                    json!({"tools": active_tool_names}),
                ),
            )),
            // ── session state ────────────────────────────────────────────
            SessionTreeEntry::Title { title, .. } => out.push(Emitted::new(
                &session,
                StateAction::SessionTitleChanged(SessionTitleChangedAction {
                    title: title.clone(),
                }),
            )),
            SessionTreeEntry::CwdChange { cwd, .. } => {
                // AHP models working directories as a *granted set*; the effective
                // directory is not a protocol field, so the grant rides the
                // membership action and the effective value rides the config model.
                let directory = file_uri(cwd);
                let fresh = !self.granted.iter().any(|seen| seen == &directory);
                if fresh {
                    self.granted.push(directory.clone());
                    out.push(Emitted::new(
                        &session,
                        StateAction::SessionWorkingDirectorySet(SessionWorkingDirectorySetAction {
                            directory,
                        }),
                    ));
                }
                out.push(Emitted::new(
                    &session,
                    config_changed(json!({config_keys::WORKING_DIRECTORY: cwd})),
                ));
            }
            SessionTreeEntry::ProjectChange { path, .. } => out.push(Emitted::new(
                &session,
                config_changed(json!({config_keys::PROJECT: path})),
            )),
            SessionTreeEntry::ModelChange {
                provider, model_id, ..
            } => out.push(Emitted::new(
                &session,
                // Canonical `{provider}/{model}` on the wire (L8).
                config_changed(json!({config_keys::MODEL: format!("{provider}/{model_id}")})),
            )),
            SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => out.push(Emitted::new(
                &session,
                config_changed(json!({config_keys::REASONING_EFFORT: thinking_level})),
            )),
            SessionTreeEntry::PermissionModeChange { mode, .. } => out.push(Emitted::new(
                &session,
                config_changed(json!({config_keys::APPROVAL_MODE: mode})),
            )),
            SessionTreeEntry::PinnedArchived {
                pinned, archived, ..
            } => {
                out.push(Emitted::new(
                    &session,
                    StateAction::SessionIsArchivedChanged(SessionIsArchivedChangedAction {
                        is_archived: *archived,
                    }),
                ));
                // AHP has no pin bit, so the pin half rides the thread
                // extension channel — the session channel's reducer ignores
                // extension actions by construction, so an envelope there is
                // never folded and never baselined.
                out.push(Emitted::new(
                    &thread,
                    extension_action(ext::actions::PINNED_CHANGED, json!({"pinned": pinned})),
                ));
            }
            SessionTreeEntry::Label { label, .. } => out.push(Emitted::new(
                &thread,
                // A `None` label rides as the empty string — the fold holds
                // `Some("")`, the same shape the v2 wire row carried (parity,
                // not a clear).
                extension_action(
                    ext::actions::LABEL_CHANGED,
                    json!({"label": label.clone().unwrap_or_default()}),
                ),
            )),
            SessionTreeEntry::SessionInfo { name, .. } => out.push(Emitted::new(
                &thread,
                // The row rides nested under `data`: the fold reads that key,
                // and a bare flatten would scatter the row's fields across the
                // action envelope where no arm looks for them.
                extension_action(
                    ext::actions::SESSION_INFO_CHANGED,
                    json!({"data": {"name": name}}),
                ),
            )),
            SessionTreeEntry::Leaf { target_id, .. } => out.push(Emitted::new(
                &thread,
                // A `None` target rides as the empty target id — the fold
                // holds `Some("")`, the same shape the v2 wire row carried
                // (parity, not a root reset).
                extension_action(
                    ext::actions::LEAF_CHANGED,
                    json!({"targetId": target_id.clone().unwrap_or_default()}),
                ),
            )),
            // ── plan ─────────────────────────────────────────────────────
            SessionTreeEntry::PlanModeChange { enabled, .. } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::PLAN_MODE_CHANGED,
                    json!({"enabled": enabled, "pending": false}),
                ),
            )),
            SessionTreeEntry::PlanModeRequest { enabled, .. } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::PLAN_MODE_CHANGED,
                    json!({"enabled": enabled, "pending": true}),
                ),
            )),
            SessionTreeEntry::PlanUpdate { snapshot, .. } => out.push(Emitted::new(
                &at,
                extension_action(ext::actions::PLAN_CHANGED, json!({"snapshot": snapshot})),
            )),
            SessionTreeEntry::PlanReview {
                state,
                plan_file,
                title,
                content,
                request_id: row_request_id,
                ..
            } => {
                // The review's two edges: a verdict is owed, or one landed.
                // The proposal opens VS Code's native plan-review card (a
                // `chat/inputRequested` whose request carries the `planReview`
                // block); the resolution closes that same part by its request
                // id on the chat face, and the plan-channel settle below is
                // the fold-visible half for clients whose turn lifecycle
                // already archived the request.
                if state == "resolved" {
                    // The settled edge is self-describing on rows the engine
                    // wrote after the field existed; projector memory covers
                    // rows written before it (the proposal replayed in this
                    // projector's own lifetime). A resolution neither names
                    // nor is remembered for cannot name its request — skip
                    // rather than emit a verdict no client can correlate.
                    let request_id = row_request_id.clone().or_else(|| self.plan_review.take());
                    // A resolution settles the remembered review too, named
                    // or not — leaving the memory standing re-emits it on any
                    // later id-less resolved row.
                    self.plan_review = None;
                    if let Some(request_id) = request_id {
                        // The settlement rides the plan channel in its own
                        // right: the chat-level `ChatInputCompleted` below only
                        // folds when the request's turn is still ACTIVE, and a
                        // plan review legitimately outlives its turn (the turn
                        // finishes the moment the plan is proposed) — the
                        // reducer then no-ops on both sides and the client's
                        // card can never retire (#88's composer lock). The
                        // plan-channel state is where the settlement is
                        // fold-visible regardless of turn lifecycle. It names
                        // no outcome — the journal edge does not carry one
                        // (approve / refine / implicit dismissal all land
                        // here) — hence "settled", not "verdict".
                        out.push(Emitted::new(
                            &plan,
                            extension_action(
                                ext::actions::PLAN_REVIEW_SETTLED,
                                plan_review_settled_payload(&request_id, plan_file.as_deref()),
                            ),
                        ));
                        out.push(Emitted::new(
                            &chat,
                            StateAction::ChatInputCompleted(ChatInputCompletedAction {
                                request_id,
                                response: ChatInputResponseKind::Accept,
                                answers: None,
                            }),
                        ));
                    } else {
                        tracing::debug!(
                            seq = row.seq,
                            "plan review resolved with no correlatable request id; no settlement emitted"
                        );
                    }
                } else {
                    let request_id = plan_review_request_id(&row.id);
                    self.plan_review = Some(request_id.clone());
                    // The plan block rides the `x-manox-plan` channel, which is
                    // the one place it can survive: AHP's `ChatInputRequest` has
                    // no `planReview` field, and a raw-JSON action cannot carry
                    // one either — `StateAction::Unknown` loses to the typed
                    // variant on any parse, and the SDK client parses every
                    // inbound action. That channel folds the payload into its
                    // own state (durable, and backfillable to a client that
                    // reconnects), which the one-shot broadcast was not.
                    out.push(Emitted::new(
                        &plan,
                        extension_action(
                            ext::actions::PLAN_VERDICT_REQUESTED,
                            plan_review_payload(
                                &request_id,
                                plan_file.as_deref(),
                                title.as_deref(),
                                content.as_deref(),
                            ),
                        ),
                    ));
                    // …and the interactive half is a real typed input request,
                    // so the client can answer it through the protocol's own
                    // `chat/inputCompleted` path.
                    out.push(Emitted::new(
                        &chat,
                        plan_review_requested(&request_id, title.as_deref()),
                    ));
                }
            }
            // ── work ─────────────────────────────────────────────────────
            SessionTreeEntry::Goal { goal, .. } => out.push(Emitted::new(
                &at,
                extension_action(ext::actions::WORK_GOAL_CHANGED, json!({"goal": goal})),
            )),
            SessionTreeEntry::BrowserSuites { suites, .. } => out.push(Emitted::new(
                &at,
                extension_action(ext::actions::WORK_BROWSER_SUITES, json!({"suites": suites})),
            )),
            SessionTreeEntry::BackgroundTask { snapshot, .. } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::WORK_BACKGROUND_TASKS,
                    json!({"snapshot": snapshot}),
                ),
            )),
            // ── metrics ──────────────────────────────────────────────────
            SessionTreeEntry::Metrics {
                metric_type, data, ..
            } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::METRICS_CHANGED,
                    json!({"kind": metric_type, "data": data}),
                ),
            )),
            // ── compaction and branch summaries ──────────────────────────
            SessionTreeEntry::Compaction {
                summary,
                tokens_before,
                retained_tail,
                first_kept_entry_id,
                ..
            } => {
                // AHP has no "replace the transcript" action, so the fold keeps
                // every pre-compaction turn and this row is the visible seam: the
                // summary renders in order and `_meta` says where history was cut
                // (§B.5's gap). The spinner edge clears here too.
                //
                // The kernel does not stamp the compacted message count as a
                // separate field (the retained tail's absence carries the
                // boundary), so the payload keeps the §C.2 best-effort 0.
                let messages_compacted: u32 = 0;
                let retained_tail_len = retained_tail.as_ref().map_or(0, Vec::len);
                out.push(Emitted::new(
                    &chat,
                    StateAction::ChatActivityChanged(ChatActivityChangedAction { activity: None }),
                ));
                mirror_activity(&chat, &session, None, &mut out);
                self.push_note(
                    &chat,
                    &row,
                    summary.clone(),
                    json!({"compaction": {
                        "messagesCompacted": messages_compacted,
                        "tokensBefore": tokens_before,
                        "retainedTail": retained_tail_len,
                        "firstKeptEntryId": first_kept_entry_id,
                    }}),
                    &mut out,
                );
            }
            SessionTreeEntry::CompactionStarted { tokens_before, .. } => {
                // Activity, not transcript: a spinner edge carries no content.
                let activity = format!("compacting ({tokens_before} tokens)");
                out.push(Emitted::new(
                    &chat,
                    StateAction::ChatActivityChanged(ChatActivityChangedAction {
                        activity: Some(activity.clone()),
                    }),
                ));
                mirror_activity(&chat, &session, Some(activity), &mut out);
            }
            SessionTreeEntry::BranchSummary { summary, .. } => self.push_note(
                &chat,
                &row,
                summary.clone(),
                json!({"branchSummary": {"text": summary}}),
                &mut out,
            ),
        }
        out
    }

    /// The `message` row group.
    fn on_message(
        &mut self,
        chat: &str,
        entry: &Envelope,
        row: MessageRow<'_>,
        out: &mut Vec<Emitted>,
    ) {
        let MessageRow {
            role,
            content,
            usage,
            origin_rpc,
            display,
        } = row;
        match role {
            "user" => {
                let mut row = json!({"role": role, "entryId": entry.id});
                if let Some(rpc) = origin_rpc {
                    row["originRpc"] = json!(rpc);
                }
                let message = Message {
                    text: blocks_text(content),
                    origin: MessageOrigin {
                        kind: MessageKind::User,
                    },
                    attachments: image_attachments(content),
                    model: None,
                    agent: None,
                    meta: Some(manox_meta(row)),
                };
                // A `user` row means one of two different things, and AHP draws
                // the distinction in the pending kind:
                //
                // - **A turn is open** — this is a manox *steer*: the text is
                //   injected into the turn already running, the row does not
                //   close it, and nothing carries it forward. It is `Steering`,
                //   which is what the running turn consumes. It must NOT join
                //   `pending_user`: that queue is drained by the next
                //   `turn_start`, so leaving it there re-injects the same text
                //   as a fresh user message once this turn ends.
                // - **No turn is open** — an ordinary queued submission, which
                //   the next `turn_start` carries as its opening message.
                let initiating = self.open.as_ref().is_some_and(|turn| {
                    turn.message_text.is_empty()
                        && !turn.streamed_text
                        && turn.text_part.is_none()
                        && turn.calls.is_empty()
                });
                let steering = self.open.is_some();
                // The initiating user row: journal order puts `turnStart`
                // before the user row, so the turn opened with an empty
                // placeholder message. When nothing has been streamed yet,
                // this row IS the opening message — re-issue the turnStarted
                // with the text (the reducer replaces the still-empty active
                // turn; no parts exist to orphan). Anything after real
                // content is a steer.
                if steering && initiating {
                    let turn_id = self.turn_id();
                    out.push(Emitted::new(
                        chat,
                        StateAction::ChatTurnStarted(ChatTurnStartedAction {
                            turn_id,
                            started_at: entry.timestamp.clone(),
                            message: message.clone(),
                            // The row's own id: the client retires its
                            // optimistic echo against it.
                            queued_message_id: Some(
                                origin_rpc
                                    .map(str::to_string)
                                    .unwrap_or_else(|| format!("m-{}", entry.id)),
                            ),
                            meta: Some(manox_meta(json!({"entryId": entry.id}))),
                        }),
                    ));
                    if let Some(turn) = self.open.as_mut() {
                        turn.message_text = message.text.clone();
                    }
                    return;
                }
                // The steer id is the client's, not one this translator mints:
                // the engine keys the injected row by it, the host's `steer`
                // intent receives it as the pending id, and the client retires
                // its optimistic echo against it. An `m-`-prefixed id invented
                // here would leave all three naming something nobody else uses,
                // so the echo and the row could never be reconciled.
                let id = match (steering, origin_rpc) {
                    (true, Some(rpc)) => rpc.to_string(),
                    _ => format!("m-{}", entry.id),
                };
                out.push(Emitted::new(
                    chat,
                    StateAction::ChatPendingMessageSet(ChatPendingMessageSetAction {
                        kind: if steering {
                            PendingMessageKind::Steering
                        } else {
                            PendingMessageKind::Queued
                        },
                        id: id.clone(),
                        message: message.clone(),
                    }),
                ));
                // Only a queued submission joins `pending_user`: the steer was
                // consumed by the turn it interrupted, and enqueueing it would
                // replay it as a later turn's opening message — retiring the
                // steering slot the host's removal still has to match.
                if !steering {
                    self.pending_user.push_back(PendingUser { id, message });
                }
            }
            "assistant" => {
                let text = blocks_text(content);
                if self.open.is_none() && usage.is_none() && text.is_empty() {
                    // Nothing to say; do not fabricate a turn for an empty row.
                    return;
                }
                self.ensure_turn(entry, chat, out);
                let streamed = self.open.as_ref().is_some_and(|turn| turn.streamed_text);
                if let Some(usage) = usage {
                    let turn_id = self.turn_id();
                    out.push(Emitted::new(
                        chat,
                        StateAction::ChatUsage(ChatUsageAction {
                            turn_id,
                            usage: UsageInfo {
                                input_tokens: whole(usage.input),
                                output_tokens: whole(usage.output),
                                model: None,
                                cache_read_tokens: whole(usage.cache_read),
                                meta: Some(manox_meta(json!({
                                    "cacheWrite": usage.cache_write,
                                    "reasoning": usage.reasoning,
                                }))),
                            },
                            meta: Some(manox_meta(json!({"entryId": entry.id}))),
                        }),
                    ));
                }
                // When deltas streamed the reply the row adds nothing but usage,
                // and stays silent about text (first-writer-wins).
                if !text.is_empty() && !streamed {
                    let turn_id = self.turn_id();
                    out.push(Emitted::new(
                        chat,
                        StateAction::ChatResponsePart(ChatResponsePartAction {
                            turn_id,
                            part: ResponsePart::Markdown(MarkdownResponsePart {
                                id: format!("p-{}", entry.id),
                                content: text,
                            }),
                            meta: Some(manox_meta(json!({"settledMessage": true}))),
                        }),
                    ));
                }
                if let Some(turn) = self.open.as_mut() {
                    turn.close_runs();
                    turn.streamed_text = false;
                }
            }
            "tool" => {
                // The model-context mirror of a call the `toolCall` / `toolResult`
                // rows already own. It reaches the wire only when the call is
                // unknown here, because then nothing else can settle it.
                if !tool_row_names_open_call(content, &self.open) {
                    let text = blocks_text(content);
                    if !text.is_empty() {
                        self.push_note(
                            chat,
                            entry,
                            text,
                            json!({"toolRow": {"blocks": content}}),
                            out,
                        );
                    }
                }
            }
            other => {
                // Kernel extension roles (`custom`) and anything newer: visible,
                // ordered, kernel JSON in `_meta`. `display: false` is the row's own
                // "model context only" flag, which clients filter on.
                let text = blocks_text(content);
                let fallback = other.to_string();
                let body = if text.is_empty() { fallback } else { text };
                self.push_note(
                    chat,
                    entry,
                    body,
                    json!({"role": other, "display": display, "blocks": content}),
                    out,
                );
            }
        }
    }

    /// `turnStart`: open the AHP turn, carrying the queued user row if one waits.
    fn on_turn_start(&mut self, chat: &str, entry: &Envelope, out: &mut Vec<Emitted>) {
        if let Some(open) = self.open.take() {
            // A `turnStart` with a turn still open means the previous turn never
            // closed (a crash between rows). Close it first: AHP's reducer replaces
            // the active turn, and its parts would vanish otherwise.
            self.finish_turn(&open, chat, Outcome::Complete, entry, out);
        }
        let carried = self.pending_user.pop_front();
        let (message, queued_message_id) = match carried {
            Some(pending) => (pending.message, Some(pending.id)),
            None => (no_user_row(entry), None),
        };
        let id = format!("t-{}", entry.id);
        out.push(Emitted::new(
            chat,
            StateAction::ChatTurnStarted(ChatTurnStartedAction {
                turn_id: id.clone(),
                started_at: entry.timestamp.clone(),
                message,
                queued_message_id,
                meta: Some(manox_meta(json!({"entryId": entry.id}))),
            }),
        ));
        self.open = Some(OpenTurn::new(id, entry.timestamp.clone(), false));
    }

    /// One streaming delta: create-then-append on the run's part.
    fn on_delta(
        &mut self,
        chat: &str,
        entry: &Envelope,
        run: Run,
        delta: &str,
        out: &mut Vec<Emitted>,
    ) {
        self.ensure_turn(entry, chat, out);
        let turn_id = self.turn_id();
        let part_id = {
            let turn = self.open.as_mut().expect("ensure_turn opened one");
            let slot = match run {
                Run::Text => {
                    turn.reasoning_part = None;
                    turn.streamed_text = true;
                    &mut turn.text_part
                }
                Run::Thinking => {
                    turn.text_part = None;
                    &mut turn.reasoning_part
                }
            };
            let opening = slot.is_none();
            if opening {
                *slot = Some(format!("p-{}", entry.id));
            }
            let part_id = slot.clone().expect("the run is open");
            Some((part_id, opening))
        }
        .expect("a turn is open");
        let (part_id, opening) = part_id;
        if opening {
            let part = match run {
                Run::Text => ResponsePart::Markdown(MarkdownResponsePart {
                    id: part_id.clone(),
                    content: String::new(),
                }),
                Run::Thinking => ResponsePart::Reasoning(ReasoningResponsePart {
                    id: part_id.clone(),
                    content: String::new(),
                }),
            };
            out.push(Emitted::new(
                chat,
                StateAction::ChatResponsePart(ChatResponsePartAction {
                    turn_id: turn_id.clone(),
                    part,
                    meta: None,
                }),
            ));
        }
        out.push(Emitted::new(
            chat,
            match run {
                Run::Text => StateAction::ChatDelta(ChatDeltaAction {
                    turn_id,
                    part_id,
                    content: delta.to_string(),
                    meta: None,
                }),
                Run::Thinking => StateAction::ChatReasoning(ChatReasoningAction {
                    turn_id,
                    part_id,
                    content: delta.to_string(),
                    meta: None,
                }),
            },
        ));
    }

    /// `toolCall`: announce and advance one call through AHP's lifecycle.
    // The row's fields are decoded positionally: the journal owns the column list
    // and grouping them would only rename it, so the lint conflicts with the
    // translation's shape here.
    #[allow(clippy::too_many_arguments)]
    fn on_tool_call(
        &mut self,
        chat: &str,
        entry: &Envelope,
        call_id: &str,
        name: &str,
        title: &str,
        status: &str,
        input: &Value,
        out: &mut Vec<Emitted>,
    ) {
        self.ensure_turn(entry, chat, out);
        let turn_id = self.turn_id();
        let first = {
            let turn = self.open.as_mut().expect("ensure_turn opened one");
            turn.close_runs();
            let fresh = !turn.calls.contains_key(call_id);
            if fresh {
                turn.calls
                    .insert(call_id.to_string(), CallBook::new(name, title));
            }
            fresh
        };
        if first {
            out.push(Emitted::new(
                chat,
                StateAction::ChatToolCallStart(ChatToolCallStartAction {
                    turn_id: turn_id.clone(),
                    tool_call_id: call_id.to_string(),
                    meta: Some(manox_meta(json!({"entryId": entry.id}))),
                    tool_name: name.to_string(),
                    display_name: name.to_string(),
                    intention: Some(title.to_string()),
                    contributor: None,
                }),
            ));
        }
        // The journal carries complete parameters, so they stream out as one delta:
        // `partialInput` is where a client shows arguments while the call is still
        // `streaming`.
        if let Some(content) = inline_input(input) {
            out.push(Emitted::new(
                chat,
                StateAction::ChatToolCallDelta(ChatToolCallDeltaAction {
                    turn_id: turn_id.clone(),
                    tool_call_id: call_id.to_string(),
                    meta: None,
                    content: Some(content),
                    invocation_message: Some(StringOrMarkdown::Plain(title.to_string())),
                }),
            ));
        }
        match status_phase(status) {
            Phase::Announced => {}
            Phase::Working => {
                // An outstanding approval owns this transition: the call stays
                // `pending-confirmation` until its verdict arrives.
                if !self.call(call_id).is_some_and(|c| c.awaiting || c.working) {
                    self.publish_running(chat, &turn_id, call_id, title, out);
                }
            }
            Phase::Finished | Phase::Failed => {
                // Do not settle yet: the journal's own `toolResult` row carries the
                // output and settles the call. If that row never comes, the turn
                // boundary settles it from what the wire does know.
                if !self.call(call_id).is_some_and(|c| c.working) {
                    self.publish_running(chat, &turn_id, call_id, title, out);
                }
            }
            Phase::Aborted => {
                if self.call(call_id).is_some_and(|c| c.awaiting) {
                    self.publish_cancelled(chat, &turn_id, call_id, status, out);
                } else {
                    self.settle(chat, &turn_id, call_id, false, out);
                }
            }
        }
    }

    /// `toolResult`: the authoritative settle of a call.
    fn on_tool_result(
        &mut self,
        chat: &str,
        entry: &Envelope,
        call_id: &str,
        output: &str,
        is_error: bool,
        out: &mut Vec<Emitted>,
    ) {
        self.ensure_turn(entry, chat, out);
        let turn_id = self.turn_id();
        self.open_call(chat, &turn_id, call_id, out);
        if !self.call(call_id).is_some_and(|c| c.working) {
            let title = self.title_of(call_id);
            self.publish_running(chat, &turn_id, call_id, &title, out);
        }
        // The result text is the call's content; accumulated stdout chunks are the
        // fallback for a row that carries none.
        if !output.is_empty()
            && let Some(call) = self.call_mut(call_id)
        {
            call.output = output.to_string();
        }
        self.settle(chat, &turn_id, call_id, !is_error, out);
    }

    /// `toolOutputChunk`: live output on a call that is still running.
    fn on_tool_output(
        &mut self,
        chat: &str,
        entry: &Envelope,
        call_id: &str,
        chunk: &str,
        out: &mut Vec<Emitted>,
    ) {
        self.ensure_turn(entry, chat, out);
        let turn_id = self.turn_id();
        self.open_call(chat, &turn_id, call_id, out);
        let (accumulated, visible) = {
            let call = self.call_mut(call_id).expect("open_call ensured one");
            call.output.push_str(chunk);
            (call.output.clone(), call.working && !call.settled)
        };
        // Only a running call can carry content, and the action replaces the array
        // — hence the accumulated text.
        if visible {
            out.push(Emitted::new(
                chat,
                StateAction::ChatToolCallContentChanged(ChatToolCallContentChangedAction {
                    turn_id,
                    tool_call_id: call_id.to_string(),
                    meta: None,
                    content: text_blocks(&accumulated),
                }),
            ));
        }
    }

    /// `approval`: the confirmation state machine, plus its session mirror.
    // See `on_tool_call` for why the row stays positional here.
    #[allow(clippy::too_many_arguments)]
    fn on_approval(
        &mut self,
        chat: &str,
        session: &str,
        entry: &Envelope,
        kind: &str,
        auth_id: &str,
        tool_name: Option<&str>,
        tool_call_id: Option<&str>,
        verdict: Option<&str>,
        reason: Option<&str>,
        out: &mut Vec<Emitted>,
    ) {
        // manox's gate registers by tool-call id, so the authId *is* the handle
        // when the row does not name one separately.
        let call_id = tool_call_id.unwrap_or(auth_id);
        let approved = matches!(verdict.unwrap_or_default(), "allow_once" | "answered");
        let turn_id = match kind {
            "request" => {
                self.ensure_turn(entry, chat, out);
                let turn_id = self.turn_id();
                self.open_call_named(chat, &turn_id, call_id, tool_name, out);
                let title = self.title_of(call_id);
                let prompt = confirmation_prompt(&title, tool_name.unwrap_or(call_id));
                out.push(Emitted::new(
                    chat,
                    StateAction::ChatToolCallReady(ChatToolCallReadyAction {
                        turn_id: turn_id.clone(),
                        tool_call_id: call_id.to_string(),
                        meta: Some(manox_meta(approval_meta(auth_id, &title))),
                        contributor: None,
                        intention: Some(title.clone()),
                        invocation_message: StringOrMarkdown::Plain(prompt.clone()),
                        // No `toolInput`: the parameters reached the wire through
                        // the call's own row, and an approval row carries none.
                        tool_input: None,
                        confirmation_title: Some(StringOrMarkdown::Plain(prompt)),
                        risk_assessment: None,
                        edits: None,
                        editable: None,
                        confirmed: None,
                        options: Some(approval_options()),
                    }),
                ));
                self.mark_awaiting(call_id);
                out.push(Emitted::new(
                    session,
                    StateAction::SessionInputNeededSet(Box::new(SessionInputNeededSetAction {
                        request: SessionInputRequest::ToolConfirmation(
                            SessionToolConfirmationRequest {
                                id: auth_id.to_string(),
                                chat: chat.to_string(),
                                turn_id: turn_id.clone(),
                                tool_call: ToolCallConfirmationState::PendingConfirmation(
                                    self.pending_confirmation(call_id, tool_name, auth_id),
                                ),
                            },
                        ),
                    })),
                ));
                turn_id
            }
            "decision" => match self.open.as_ref() {
                Some(turn) => turn.id.clone(),
                None => {
                    // A verdict for a turn that already closed: the boundary
                    // settled or cancelled the call, so re-folding it here would
                    // move state the journal no longer describes.
                    tracing::debug!(auth_id, "approval verdict after the turn closed");
                    return;
                }
            },
            // A request is not a decision: it only opens the gate. Emitting a
            // confirmation for it would cancel the very call the user is being
            // asked about (the verdict is absent, so it reads as a denial) and
            // settle it before its result row arrives.
            other => {
                tracing::debug!(kind = other, "unrecognised approval kind");
                return;
            }
        };
        if kind == "request" {
            // The gate is open; the verdict that closes it is a later row.
            return;
        }
        out.push(Emitted::new(
            chat,
            StateAction::ChatToolCallConfirmed(ChatToolCallConfirmedAction {
                turn_id,
                tool_call_id: call_id.to_string(),
                meta: Some(manox_meta(approval_meta(
                    auth_id,
                    reason.unwrap_or_default(),
                ))),
                approved,
                confirmed: approved.then_some(ToolCallConfirmationReason::UserAction),
                reason: (!approved).then_some(match verdict.unwrap_or_default() {
                    "deny" => ToolCallCancellationReason::Denied,
                    _ => ToolCallCancellationReason::Skipped,
                }),
                edited_tool_input: None,
                user_suggestion: None,
                reason_message: reason.map(|text| StringOrMarkdown::Plain(text.to_string())),
                // The verdict string *is* the option id in manox's vocabulary.
                selected_option_id: (!approved).then(|| verdict.map(str::to_string)).flatten(),
            }),
        ));
        // The session mirror closes with the chat-level transition, whether or not
        // the call itself could move.
        out.push(Emitted::new(
            session,
            StateAction::SessionInputNeededRemoved(SessionInputNeededRemovedAction {
                id: auth_id.to_string(),
            }),
        ));
        self.mark_resolved(call_id, approved);
    }

    /// `question`: AHP's own elicitation plane.
    fn on_question(
        &mut self,
        chat: &str,
        entry: &Envelope,
        ask: AskRow<'_>,
        out: &mut Vec<Emitted>,
    ) {
        let AskRow {
            kind,
            auth_id,
            tool_name,
            verdict,
            reason,
            input,
        } = ask;
        match kind {
            "request" => {
                self.ensure_turn(entry, chat, out);
                let message = tool_name
                    .unwrap_or("the agent is asking a question")
                    .to_string();
                out.push(Emitted::new(
                    chat,
                    StateAction::ChatInputRequested(ChatInputRequestedAction {
                        request: ChatInputRequest {
                            id: auth_id.to_string(),
                            message: Some(message),
                            url: None,
                            // The structured questions ride the durable row's
                            // `input` payload (`{questions: [...]}`) and are folded
                            // into the elicitation's question list here; an older
                            // journal row without the payload still answers with
                            // the bare ask.
                            questions: input.and_then(map_ask_questions),
                            answers: None,
                        },
                    }),
                ));
            }
            "decision" => {
                if self.open.is_none() {
                    tracing::debug!(auth_id, "question verdict after the turn closed");
                    return;
                }
                let response = match verdict.unwrap_or_default() {
                    "answered" => ChatInputResponseKind::Accept,
                    "dismissed" => ChatInputResponseKind::Decline,
                    _ => ChatInputResponseKind::Cancel,
                };
                out.push(Emitted::new(
                    chat,
                    StateAction::ChatInputCompleted(ChatInputCompletedAction {
                        request_id: auth_id.to_string(),
                        response,
                        // The answer text is not journalled; a client that answered
                        // through the wire keeps its answers on the part.
                        answers: None,
                    }),
                ));
                if let Some(text) = reason {
                    self.push_note(
                        chat,
                        entry,
                        text.to_string(),
                        json!({"question": {"authId": auth_id, "reason": text}}),
                        out,
                    );
                }
            }
            other => {
                tracing::debug!(kind = other, "unrecognised question kind");
            }
        }
    }

    // ── turn and call mechanics ──────────────────────────────────────────

    /// Close the open turn, settling anything the journal left moving.
    fn close_turn(
        &mut self,
        chat: &str,
        entry: &Envelope,
        outcome: Outcome,
        out: &mut Vec<Emitted>,
    ) {
        let Some(open) = self.open.take() else {
            return;
        };
        self.finish_turn(&open, chat, outcome, entry, out);
    }

    fn finish_turn(
        &mut self,
        open: &OpenTurn,
        chat: &str,
        outcome: Outcome,
        entry: &Envelope,
        out: &mut Vec<Emitted>,
    ) {
        let duration = elapsed_ms(&open.started_at, &entry.timestamp);
        let failed = matches!(outcome, Outcome::Failed(_));
        let mut meta = json!({"entryId": entry.id, "outcome": outcome.tag()});
        if open.synthetic {
            meta["synthetic"] = json!(true);
        }
        // A call still moving at the boundary must not hang. An unanswered
        // confirmation cancels (no verdict ever arrives); anything merely running
        // completes with the output the wire collected, because the journal said
        // the loop moved on.
        for (id, call) in &open.calls {
            if call.settled {
                continue;
            }
            if call.awaiting {
                out.push(Emitted::new(
                    chat,
                    StateAction::ChatToolCallConfirmed(ChatToolCallConfirmedAction {
                        turn_id: open.id.clone(),
                        tool_call_id: id.clone(),
                        meta: Some(manox_meta(json!({"settledAt": "turnBoundary"}))),
                        approved: false,
                        confirmed: None,
                        reason: Some(ToolCallCancellationReason::Skipped),
                        edited_tool_input: None,
                        user_suggestion: None,
                        reason_message: Some(StringOrMarkdown::Plain(
                            "the turn ended before a verdict arrived".to_string(),
                        )),
                        selected_option_id: None,
                    }),
                ));
                continue;
            }
            if !call.working {
                continue;
            }
            out.push(Emitted::new(
                chat,
                StateAction::ChatToolCallComplete(ChatToolCallCompleteAction {
                    turn_id: open.id.clone(),
                    tool_call_id: id.clone(),
                    meta: Some(manox_meta(json!({"settledAt": "turnBoundary"}))),
                    result: ToolCallResult {
                        success: !failed,
                        past_tense_message: StringOrMarkdown::Plain(call.title.clone()),
                        content: (!call.output.is_empty()).then(|| text_blocks(&call.output)),
                        structured_content: None,
                        error: None,
                    },
                    requires_result_confirmation: None,
                }),
            ));
        }
        let action = match outcome {
            Outcome::Failed(info) => StateAction::ChatError(ChatErrorAction {
                turn_id: open.id.clone(),
                duration,
                part: ErrorResponsePart {
                    error: info,
                    resumable: None,
                },
                meta: Some(manox_meta(meta)),
            }),
            Outcome::Cancelled => StateAction::ChatTurnCancelled(ChatTurnCancelledAction {
                turn_id: open.id.clone(),
                duration,
                meta: Some(manox_meta(meta)),
            }),
            Outcome::Complete => StateAction::ChatTurnComplete(ChatTurnCompleteAction {
                turn_id: open.id.clone(),
                duration,
                meta: Some(manox_meta(meta)),
            }),
        };
        out.push(Emitted::new(chat, action));
    }

    /// Push a `systemNotification` response part.
    ///
    /// AHP hangs every response part on a turn, so a durable row that arrives with
    /// no open turn mints a synthetic one rather than being dropped: the fold then
    /// still shows the fact, in journal order.
    fn push_note(
        &mut self,
        chat: &str,
        entry: &Envelope,
        text: String,
        meta: Value,
        out: &mut Vec<Emitted>,
    ) {
        if let Some(turn) = self.open.as_mut() {
            turn.close_runs();
        }
        self.ensure_turn(entry, chat, out);
        let turn_id = self.turn_id();
        let mut payload = meta;
        if let Some(object) = payload.as_object_mut() {
            object.insert("entryId".to_string(), json!(entry.id));
        }
        out.push(Emitted::new(
            chat,
            StateAction::ChatResponsePart(ChatResponsePartAction {
                turn_id,
                part: ResponsePart::SystemNotification(SystemNotificationResponsePart {
                    content: StringOrMarkdown::Plain(text),
                    meta: Some(manox_meta(payload)),
                }),
                meta: None,
            }),
        ));
    }

    /// Open a turn if none is open, publishing the synthetic `turnStarted`.
    fn ensure_turn(&mut self, entry: &Envelope, chat: &str, out: &mut Vec<Emitted>) {
        if self.open.is_some() {
            return;
        }
        let id = format!("t-{}", entry.id);
        // Manox journals carry no `turnStart` rows: the queued submission
        // (the user row that preceded this content) IS the opening message —
        // without this, every replayed synthetic turn carried an empty
        // message and the transcript lost its user bubbles.
        let carried = self.pending_user.pop_front();
        let (message, queued_message_id) = match carried {
            Some(pending) => (pending.message, Some(pending.id)),
            None => (no_user_row(entry), None),
        };
        out.push(Emitted::new(
            chat,
            StateAction::ChatTurnStarted(ChatTurnStartedAction {
                turn_id: id.clone(),
                started_at: entry.timestamp.clone(),
                message,
                queued_message_id,
                meta: Some(manox_meta(json!({"synthetic": true, "entryId": entry.id}))),
            }),
        ));
        self.open = Some(OpenTurn::new(id, entry.timestamp.clone(), true));
    }

    /// Resume the turn bookkeeping for a turn the client already opened, without
    /// publishing a `turnStarted`: the client's own dispatch already set the
    /// active turn, and a synthetic `turnStarted` would replace it (the reducer
    /// overwrites `active_turn` unconditionally), severing the id the streamed
    /// parts must address.
    pub fn open_with_id(&mut self, id: String, started_at: String) {
        if self.open.is_some() {
            return;
        }
        self.open = Some(OpenTurn::new(id, started_at, false));
    }

    /// Announce a call the wire has not seen, so a later action has a part to move.
    /// Reached only on journals that record a result without a call row.
    fn open_call(&mut self, chat: &str, turn_id: &str, call_id: &str, out: &mut Vec<Emitted>) {
        self.open_call_named(chat, turn_id, call_id, None, out);
    }

    fn open_call_named(
        &mut self,
        chat: &str,
        turn_id: &str,
        call_id: &str,
        tool_name: Option<&str>,
        out: &mut Vec<Emitted>,
    ) {
        if self.call(call_id).is_some() {
            return;
        }
        out.push(Emitted::new(
            chat,
            StateAction::ChatToolCallStart(ChatToolCallStartAction {
                turn_id: turn_id.to_string(),
                tool_call_id: call_id.to_string(),
                meta: Some(manox_meta(json!({"inferred": true}))),
                tool_name: tool_name.unwrap_or_default().to_string(),
                display_name: tool_name.unwrap_or_default().to_string(),
                intention: None,
                contributor: None,
            }),
        ));
        let turn = self.open.as_mut().expect("a turn is open");
        turn.calls
            .insert(call_id.to_string(), CallBook::new("", ""));
    }

    fn publish_running(
        &mut self,
        chat: &str,
        turn_id: &str,
        call_id: &str,
        title: &str,
        out: &mut Vec<Emitted>,
    ) {
        out.push(Emitted::new(
            chat,
            StateAction::ChatToolCallReady(ChatToolCallReadyAction {
                turn_id: turn_id.to_string(),
                tool_call_id: call_id.to_string(),
                meta: None,
                contributor: None,
                intention: Some(title.to_string()),
                invocation_message: StringOrMarkdown::Plain(title.to_string()),
                tool_input: None,
                confirmation_title: None,
                risk_assessment: None,
                edits: None,
                editable: None,
                // The journal says it ran, so confirmation was not needed — or it
                // landed on an approval row, which owns that path.
                confirmed: Some(ToolCallConfirmationReason::NotNeeded),
                options: None,
            }),
        ));
        if let Some(call) = self.call_mut(call_id) {
            call.working = true;
            call.awaiting = false;
        }
    }

    fn publish_cancelled(
        &mut self,
        chat: &str,
        turn_id: &str,
        call_id: &str,
        status: &str,
        out: &mut Vec<Emitted>,
    ) {
        out.push(Emitted::new(
            chat,
            StateAction::ChatToolCallConfirmed(ChatToolCallConfirmedAction {
                turn_id: turn_id.to_string(),
                tool_call_id: call_id.to_string(),
                meta: None,
                approved: false,
                confirmed: None,
                reason: Some(match status {
                    "denied" => ToolCallCancellationReason::Denied,
                    _ => ToolCallCancellationReason::Skipped,
                }),
                edited_tool_input: None,
                user_suggestion: None,
                reason_message: None,
                selected_option_id: None,
            }),
        ));
        if let Some(call) = self.call_mut(call_id) {
            call.awaiting = false;
            call.settled = true;
        }
    }

    fn settle(
        &mut self,
        chat: &str,
        turn_id: &str,
        call_id: &str,
        success: bool,
        out: &mut Vec<Emitted>,
    ) {
        let (already, title, output) = match self.call(call_id) {
            Some(call) => (call.settled, call.title.clone(), call.output.clone()),
            None => return,
        };
        if already {
            return;
        }
        out.push(Emitted::new(
            chat,
            StateAction::ChatToolCallComplete(ChatToolCallCompleteAction {
                turn_id: turn_id.to_string(),
                tool_call_id: call_id.to_string(),
                meta: None,
                result: ToolCallResult {
                    success,
                    past_tense_message: StringOrMarkdown::Plain(title),
                    content: (!output.is_empty()).then(|| text_blocks(&output)),
                    structured_content: None,
                    error: None,
                },
                requires_result_confirmation: None,
            }),
        ));
        if let Some(call) = self.call_mut(call_id) {
            call.settled = true;
            call.awaiting = false;
        }
    }

    fn mark_awaiting(&mut self, call_id: &str) {
        if let Some(call) = self.call_mut(call_id) {
            call.awaiting = true;
            call.working = false;
        }
    }

    fn mark_resolved(&mut self, call_id: &str, approved: bool) {
        if let Some(call) = self.call_mut(call_id) {
            call.awaiting = false;
            if approved {
                call.working = true;
            } else {
                call.settled = true;
            }
        }
    }

    fn pending_confirmation(
        &self,
        call_id: &str,
        tool_name: Option<&str>,
        auth_id: &str,
    ) -> ToolCallPendingConfirmationState {
        let call = self.call(call_id);
        let name = tool_name
            .or_else(|| call.map(|c| c.tool_name.as_str()))
            .unwrap_or_default();
        let title = call.map(|c| c.title.clone()).unwrap_or_default();
        ToolCallPendingConfirmationState {
            tool_call_id: call_id.to_string(),
            tool_name: name.to_string(),
            display_name: name.to_string(),
            intention: (!title.is_empty()).then_some(title.clone()),
            contributor: None,
            meta: Some(manox_meta(approval_meta(auth_id, &title))),
            invocation_message: StringOrMarkdown::Plain(confirmation_prompt(&title, name)),
            tool_input: None,
            confirmation_title: None,
            risk_assessment: None,
            edits: None,
            editable: None,
            options: Some(approval_options()),
        }
    }

    fn call(&self, call_id: &str) -> Option<&CallBook> {
        self.open.as_ref()?.calls.get(call_id)
    }

    fn call_mut(&mut self, call_id: &str) -> Option<&mut CallBook> {
        self.open.as_mut()?.calls.get_mut(call_id)
    }

    fn title_of(&self, call_id: &str) -> String {
        self.call(call_id)
            .map(|call| call.title.clone())
            .unwrap_or_default()
    }

    fn turn_id(&self) -> String {
        self.open
            .as_ref()
            .map(|turn| turn.id.clone())
            .unwrap_or_default()
    }
}

/// The message that carries a turn with no user row of its own.
///
/// Retries, auto-started turns and synthetic boundaries all look like this on the
/// wire; `_meta` says which, so a client never renders a phantom prompt.
fn no_user_row(entry: &Envelope) -> Message {
    Message {
        text: String::new(),
        origin: MessageOrigin {
            kind: MessageKind::User,
        },
        attachments: None,
        model: None,
        agent: None,
        meta: Some(manox_meta(json!({"noUserRow": true, "entryId": entry.id}))),
    }
}

/// The pending-confirmation option set.
///
/// manox's gate answers with a verdict string, and these ids *are* that
/// vocabulary, so a client that renders them still round-trips honestly.
fn approval_options() -> Vec<ConfirmationOption> {
    vec![
        ConfirmationOption {
            id: "allow_once".to_string(),
            label: "Allow once".to_string(),
            kind: ConfirmationOptionKind::Approve,
            group: Some(0),
        },
        ConfirmationOption {
            id: "deny".to_string(),
            label: "Deny".to_string(),
            kind: ConfirmationOptionKind::Deny,
            group: Some(1),
        },
    ]
}

/// The `_meta["x-manox"]` payload of a confirmation.
///
/// `summary` and `deliveryId` are §B.3's reconciliation face; the durable row
/// carries neither (they belong to the gateway's delivery, not the journal), so
/// only what the journal knows travels here and the empty seats stay visible.
fn approval_meta(auth_id: &str, summary: &str) -> Value {
    json!({
        "authId": auth_id,
        "summary": summary,
        "deliveryId": null,
    })
}

fn confirmation_prompt(title: &str, tool_name: &str) -> String {
    let subject = if title.trim().is_empty() {
        tool_name
    } else {
        title
    };
    format!("{subject} — awaiting confirmation")
}

/// The plan-review block, for the `x-manox-plan` channel.
///
/// This is where the plan's title, content and actions live. It cannot ride
/// `chat/inputRequested`: AHP's `ChatInputRequest` has no `planReview` field,
/// and a raw-JSON action cannot smuggle one in either — `StateAction::Unknown`
/// is an untagged fallback, so any parse resolves the tag to the *typed*
/// variant and drops the fields that variant does not declare. The SDK client
/// parses every inbound action, so the block was being stripped on arrival.
///
/// The extension channel takes it instead: it folds the payload into its own
/// state (so a client reads it back from the channel baseline, and a
/// reconnecting client is backfilled), which a one-shot broadcast could not do.
fn plan_review_payload(
    request_id: &str,
    plan_file: Option<&str>,
    title: Option<&str>,
    content: Option<&str>,
) -> Value {
    let title = title
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("Review Plan");
    let content = content
        .filter(|c| !c.trim().is_empty())
        .unwrap_or("A plan is ready for review.");
    let mut payload = json!({
        "requestId": request_id,
        "title": title,
        "content": content,
        "actions": [
            { "id": "approve", "label": "Approve", "default": true },
            { "id": "refine", "label": "Refine" },
        ],
        "canProvideFeedback": true,
    });
    if let Some(plan_file) = plan_file {
        payload["planUri"] = json!(file_uri(plan_file));
    }
    payload
}

/// The settled edge's payload: the same `planUri` spelling and encoding the
/// proposal edge used (the fold replaces `plan_review` wholesale, so the pair
/// must speak one shape or a late subscriber sees the plan reference mutate).
/// The key is omitted — not null — when the journal row carried no file, as
/// `plan_review_payload` does.
fn plan_review_settled_payload(request_id: &str, plan_file: Option<&str>) -> Value {
    let mut payload = json!({ "requestId": request_id });
    if let Some(plan_file) = plan_file {
        payload["planUri"] = json!(file_uri(plan_file));
    }
    payload
}

/// The interactive half: a typed `chat/inputRequested` asking the user to
/// approve or refine.
///
/// Typed deliberately, and only the question rides it. AHP folds this into the
/// turn as an `InputRequest` response part, so the client can answer through
/// `chat/inputCompleted` and the request is durable and backfillable — none of
/// which holds for a hand-rolled action. The plan's *content* is not here
/// because the protocol has no field for it; it rides
/// [`plan_review_payload`].
fn plan_review_requested(request_id: &str, title: Option<&str>) -> StateAction {
    let question_id = format!("{request_id}:q");
    let title = title
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("Review Plan");
    StateAction::ChatInputRequested(ChatInputRequestedAction {
        request: ChatInputRequest {
            id: request_id.to_string(),
            message: Some("A plan is ready for review.".to_string()),
            url: None,
            questions: Some(vec![ChatInputQuestion::SingleSelect(
                ChatInputSingleSelectQuestion {
                    id: question_id,
                    title: Some(title.to_string()),
                    message: "How would you like to proceed?".to_string(),
                    required: Some(true),
                    options: vec![
                        ChatInputOption {
                            id: "approve".to_string(),
                            label: "Approve".to_string(),
                            description: None,
                            recommended: Some(true),
                        },
                        ChatInputOption {
                            id: "refine".to_string(),
                            label: "Refine".to_string(),
                            description: None,
                            recommended: None,
                        },
                    ],
                    allow_freeform_input: Some(true),
                },
            )]),
            answers: None,
        },
    })
}

/// `{"x-manox": value}` — the one private metadata seat this crate writes.
fn manox_meta(value: Value) -> JsonObject {
    let mut meta = JsonObject::new();
    meta.insert(ext::META_KEY.to_string(), value);
    meta
}

/// An action outside AHP's type space, on the declared `x-manox` surface.
///
/// `ahp-types` mints no Rust shape for extension actions, so the wire form *is*
/// the payload's JSON plus the declared tag; [`ext::actions`] stays the naming
/// authority and [`ext::accepts_action`] gates it.
fn extension_action(kind: &str, payload: Value) -> StateAction {
    let mut fields = match payload {
        Value::Object(map) => map,
        other => {
            let mut map = JsonObject::new();
            map.insert("value".to_string(), other);
            map
        }
    };
    let mut tagged = JsonObject::new();
    tagged.insert("type".to_string(), Value::String(kind.to_string()));
    tagged.append(&mut fields);
    StateAction::Unknown(Value::Object(tagged))
}

fn config_changed(values: Value) -> StateAction {
    StateAction::SessionConfigChanged(SessionConfigChangedAction {
        config: match values {
            Value::Object(map) => map,
            _ => JsonObject::new(),
        },
        replace: None,
    })
}

/// Join a kernel message's text blocks.
///
/// The §C.2 storage shape is wire-opaque; only `text` blocks have a
/// protocol-visible rendering, and the rest reach clients through `_meta`.
fn blocks_text(content: &[Value]) -> String {
    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Image blocks become embedded-resource attachments on the AHP message.
fn image_attachments(content: &[Value]) -> Option<Vec<MessageAttachment>> {
    let images: Vec<MessageAttachment> = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .map(|block| {
            MessageAttachment::EmbeddedResource(MessageEmbeddedResourceAttachment {
                label: "image".to_string(),
                range: None,
                display_kind: Some("image".to_string()),
                meta: None,
                data: block
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                content_type: block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                selection: None,
            })
        })
        .collect();
    (!images.is_empty()).then_some(images)
}

/// The human-readable text of a note-shaped row, when it carries one.
fn note_text(data: &Value) -> Option<String> {
    ["text", "message", "summary"]
        .iter()
        .find_map(|key| data.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

fn text_blocks(text: &str) -> Vec<ToolResultContent> {
    vec![ToolResultContent::Text(ToolResultTextContent {
        text: text.to_string(),
    })]
}

/// The tool-call handle of any lifecycle state.
fn tool_call_handle(state: &ToolCallState) -> &str {
    match state {
        ToolCallState::Streaming(s) => &s.tool_call_id,
        ToolCallState::PendingConfirmation(s) => &s.tool_call_id,
        ToolCallState::Running(s) => &s.tool_call_id,
        ToolCallState::AuthRequired(s) => &s.tool_call_id,
        ToolCallState::PendingResultConfirmation(s) => &s.tool_call_id,
        ToolCallState::Completed(s) => &s.tool_call_id,
        ToolCallState::Cancelled(s) => &s.tool_call_id,
        ToolCallState::Unknown(_) => "",
    }
}

fn tool_call_intention(state: &ToolCallState) -> Option<String> {
    match state {
        ToolCallState::Streaming(s) => s.intention.clone(),
        ToolCallState::PendingConfirmation(s) => s.intention.clone(),
        ToolCallState::Running(s) => s.intention.clone(),
        ToolCallState::AuthRequired(s) => s.intention.clone(),
        ToolCallState::PendingResultConfirmation(s) => s.intention.clone(),
        ToolCallState::Completed(s) => s.intention.clone(),
        ToolCallState::Cancelled(s) => s.intention.clone(),
        ToolCallState::Unknown(_) => None,
    }
}

/// Whether a `tool` transcript row names a call the open turn already tracks.
fn tool_row_names_open_call(content: &[Value], open: &Option<OpenTurn>) -> bool {
    let Some(turn) = open else {
        return false;
    };
    content.iter().any(|block| {
        block
            .get("toolCallId")
            .and_then(Value::as_str)
            .is_some_and(|id| turn.calls.contains_key(id))
    })
}

/// Journal tool input as AHP's inline parameter text.
fn inline_input(input: &Value) -> Option<String> {
    match input {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// A directory path as the `file://` URI AHP's working-directory fields want.
fn file_uri(path: &str) -> Uri {
    if path.contains("://") {
        return path.to_string();
    }
    format!("file:///{}", path.trim_start_matches('/'))
}

/// Turn duration from two journal timestamps, in milliseconds.
///
/// Unparseable timestamps yield `0`: AHP's reducer validates the active turn's
/// `startedAt` itself, so a bad stamp is reported there rather than hidden here.
fn elapsed_ms(start: &str, end: &str) -> i64 {
    let parse = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|stamp| stamp.timestamp_millis())
    };
    match (parse(start), parse(end)) {
        (Some(from), Some(to)) if to >= from => to - from,
        _ => 0,
    }
}

/// A journal token counter onto AHP's `i64` fields.
fn whole(count: u64) -> Option<i64> {
    i64::try_from(count).ok()
}

/// One kernel record from its JSON shape — the same rows the jsonl store
/// persists, so the fixtures below speak the kernel vocabulary end to end.
/// The base stamps (id / parent / timestamp) apply unless the fields object
/// overrides them.
#[cfg(test)]
fn kernel(mut fields: Value) -> SessionTreeEntry {
    let mut base = json!({
        "id": "e-1",
        "parentId": None::<String>,
        "timestamp": "2026-09-27T00:00:00.000Z",
    });
    let object = base.as_object_mut().expect("base is an object");
    if let Some(extra) = fields.as_object_mut() {
        object.append(extra);
    }
    serde_json::from_value(base.clone())
        .unwrap_or_else(|e| panic!("kernel fixture failed: {e} — {base}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_activity_edge_mirrors_onto_the_session_channel_twice() {
        let mut translator = Translator::new();
        let emitted = translator.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({"type": "compaction_started", "tokensBefore": 4200})),
        );
        let session: Vec<_> = emitted
            .iter()
            .filter(|e| e.channel == "ahp-session:/s-1")
            .collect();
        assert_eq!(
            session.len(),
            2,
            "the set edge owes the session-level string and the catalogue delta: {session:?}"
        );
        let activity = serde_json::to_value(&session[0].action).unwrap();
        assert_eq!(activity["type"], "session/activityChanged");
        assert_eq!(activity["activity"], "compacting (4200 tokens)");
        let catalog = serde_json::to_value(&session[1].action).unwrap();
        assert_eq!(catalog["type"], "session/chatUpdated");
        assert_eq!(catalog["chat"], "ahp-chat:/c-1");
        assert_eq!(catalog["changes"]["activity"], "compacting (4200 tokens)");
        // The chat-level edge itself is unchanged.
        let chat = emitted
            .iter()
            .find(|e| e.channel == "ahp-chat:/c-1")
            .expect("the chat edge stays");
        assert_eq!(
            serde_json::to_value(&chat.action).unwrap()["type"],
            "chat/activityChanged"
        );
    }

    #[test]
    fn a_cleared_activity_edge_resets_the_session_string_but_not_the_catalog() {
        let mut translator = Translator::new();
        let emitted = translator.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({"type": "compaction", "summary": "done", "tokensBefore": 10})),
        );
        let session: Vec<_> = emitted
            .iter()
            .filter(|e| e.channel == "ahp-session:/s-1")
            .collect();
        assert_eq!(
            session.len(),
            1,
            "a cleared edge owes only the session-level reset: {session:?}"
        );
        let activity = serde_json::to_value(&session[0].action).unwrap();
        assert_eq!(activity["type"], "session/activityChanged");
        assert_eq!(
            activity["activity"],
            serde_json::Value::Null,
            "None serializes as an absent/null activity — the reducer's clear edge"
        );
    }

    #[test]
    fn plan_review_proposal_splits_content_from_the_question() {
        let mut translator = Translator::new();
        let emitted = translator.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({
                "type": "plan_review",
                "state": "proposed",
                "planFile": "/plans/demo-plan.md",
                "title": "Demo plan",
                "content": "# Demo\n\n- step one",
            })),
        );
        assert_eq!(
            emitted.len(),
            2,
            "the proposal emits the plan block and the question: {emitted:?}"
        );

        // The plan's own content rides the extension channel — the only place
        // it survives a parse, and the only place a reconnect is backfilled
        // from.
        let plan = emitted
            .iter()
            .find(|e| e.channel == "x-manox-plan:/c-1")
            .expect("the plan block rides the plan channel");
        let value = serde_json::to_value(&plan.action).expect("action serializes");
        assert_eq!(value["type"], "x-manox-plan/verdictRequested");
        assert_eq!(value["requestId"], "plan-review:e-1");
        assert_eq!(value["title"], "Demo plan");
        assert_eq!(value["content"], "# Demo\n\n- step one");
        assert_eq!(value["actions"][0]["id"], "approve");
        assert_eq!(value["actions"][1]["id"], "refine");
        assert_eq!(value["canProvideFeedback"], true);
        assert!(
            value["planUri"]
                .as_str()
                .is_some_and(|u| u.contains("demo-plan")),
            "the block names the reviewed plan: {value}"
        );

        // The question is a real typed `chat/inputRequested`, so the client
        // answers it through the protocol's own completion path and the host
        // folds it into the turn.
        let card = emitted
            .iter()
            .find(|e| e.channel == "ahp-chat:/c-1")
            .expect("the question rides the chat channel");
        let card = serde_json::to_value(&card.action).expect("action serializes");
        assert_eq!(card["type"], "chat/inputRequested");
        assert_eq!(card["request"]["id"], "plan-review:e-1");
        assert_eq!(card["request"]["questions"][0]["kind"], "single-select");
        assert_eq!(card["request"]["questions"][0]["id"], "plan-review:e-1:q");
        assert_eq!(
            card["request"]["questions"][0]["options"][0]["id"],
            "approve"
        );
    }

    #[test]
    fn plan_review_resolution_closes_the_card() {
        let mut translator = Translator::new();
        translator.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({
                "type": "plan_review",
                "state": "proposed",
                "planFile": "/p.md",
                "title": "T",
                "content": "# T",
            })),
        );
        let emitted = translator.on_entry(
            "c-1",
            "s-1",
            1,
            &kernel(json!({"type": "plan_review", "id": "e-2", "state": "resolved"})),
        );
        assert_eq!(emitted.len(), 2);
        let settled = serde_json::to_value(&emitted[0].action).expect("action serializes");
        assert_eq!(settled["type"], ext::actions::PLAN_REVIEW_SETTLED);
        assert_eq!(settled["requestId"], "plan-review:e-1");
        // No file on the resolved edge: the key is OMITTED — the same
        // `planUri` spelling and absence semantics the proposal edge uses
        // (the fold replaces `plan_review` wholesale, so the pair must
        // speak one shape).
        assert!(emitted[0].channel.starts_with("x-manox-plan"));
        assert!(settled.get("planUri").is_none());
        let value = serde_json::to_value(&emitted[1].action).expect("action serializes");
        assert_eq!(value["type"], "chat/inputCompleted");
        assert_eq!(value["requestId"], "plan-review:e-1");
    }

    /// The resolution is self-describing: a resolved row carrying its
    /// request id settles in a projector that never saw the proposal (a
    /// bridge resuming above it, after a host restart). The payload names
    /// the plan with the proposal edge's `planUri` encoding.
    #[test]
    fn plan_review_resolution_survives_a_projector_restart() {
        let mut translator = Translator::new();
        let emitted = translator.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({
                "type": "plan_review",
                "id": "e-9",
                "state": "resolved",
                "planFile": "/p.md",
                "requestId": "plan-review:e-1",
            })),
        );
        assert_eq!(emitted.len(), 2);
        let settled = serde_json::to_value(&emitted[0].action).expect("action serializes");
        assert_eq!(settled["type"], ext::actions::PLAN_REVIEW_SETTLED);
        assert_eq!(settled["requestId"], "plan-review:e-1");
        assert_eq!(settled["planUri"], "file:///p.md");
        let value = serde_json::to_value(&emitted[1].action).expect("action serializes");
        assert_eq!(value["requestId"], "plan-review:e-1");
    }

    /// A resolution that neither names its request nor is remembered for
    /// one cannot be correlated — nothing is emitted rather than a verdict
    /// no client can route.
    #[test]
    fn plan_review_resolution_without_a_correlatable_id_emits_nothing() {
        let mut translator = Translator::new();
        let emitted = translator.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({"type": "plan_review", "id": "e-9", "state": "resolved"})),
        );
        assert!(emitted.is_empty());
    }

    #[test]
    fn a_background_task_row_rides_the_snapshot_key_into_the_registry_state() {
        // One-field payload on purpose: this test pins the wire KEY (the row
        // rides nested under `snapshot`) and its route into the fold — not
        // the row's shape, which no literal here could faithfully claim. The
        // shape's authority is the host golden
        // (`task_snapshot_serialization_is_wire_stable`), and the chain from
        // that real shape runs in manox-ahp-runtime's
        // `background_task_snapshot_flows_from_host_shape_to_extension_state`.
        let snapshot = json!({"task_id": "mon_7"});
        let mut translator = Translator::new();
        let emitted = translator.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({"type": "background_task", "snapshot": snapshot.clone()})),
        );
        let work: Vec<_> = emitted
            .iter()
            .filter(|e| e.channel == "x-manox-work:/s-1")
            .collect();
        assert_eq!(
            work.len(),
            1,
            "one kernel row, one work-channel action: {emitted:?}"
        );
        let action = serde_json::to_value(&work[0].action).expect("action serializes");
        assert_eq!(action["type"], "x-manox-work/backgroundTasksChanged");
        assert_eq!(
            action["snapshot"], snapshot,
            "the canonical payload key is `snapshot`"
        );

        // The reference client fold: the row lands in the registry view
        // keyed by its task id.
        let mut state = ext::XManoxState::default();
        assert_eq!(
            ext::reducer::apply(&mut state, &action),
            ext::ExtOutcome::Applied
        );
        assert_eq!(
            state.background_tasks,
            Some(
                [("mon_7".to_string(), snapshot.clone())]
                    .into_iter()
                    .collect()
            )
        );
        // …and the field survives a state round-trip for reconnecting clients.
        let encoded = serde_json::to_value(&state).expect("state serializes");
        assert_eq!(encoded["backgroundTasks"]["mon_7"], snapshot);
        let decoded: ext::XManoxState = serde_json::from_value(encoded).expect("state round-trips");
        assert_eq!(decoded.background_tasks, state.background_tasks);
    }

    /// The full background-task projection chain, anchored at the real
    /// producer shape: host `TaskSnapshot` → kernel row → projected action →
    /// extension registry state. Lives here because this is the only crate
    /// that may name both `manox_agent` (the shape's authority) and
    /// `manox_ahp` (the fold) — the golden in `manox-agent` pins the
    /// serialization, this test pins that everything downstream of it still
    /// consumes that exact shape.
    #[test]
    fn background_task_snapshot_flows_from_host_shape_to_extension_state() {
        use manox_agent::background_task::{TaskKind, TaskSnapshot, TaskStatus};

        let snapshot = TaskSnapshot {
            task_id: "mon_7".into(),
            kind: TaskKind::MonitorCommand,
            owner_thread_id: "t-1".into(),
            description: "watch the build".into(),
            status: TaskStatus::Completed,
            created_at_ms: 1_000,
            ended_at_ms: Some(2_000),
            event_count: 42,
            total_bytes: 4_096,
            exit_code: Some(0),
            failure_summary: Some("nope".into()),
            anchor_message_id: Some("m1".into()),
            output_tail: "last line".into(),
        };
        let wire = serde_json::to_value(&snapshot).expect("snapshot serializes");
        let other = TaskSnapshot {
            task_id: "bg_8".into(),
            kind: TaskKind::BackgroundBash,
            description: "run the suite".into(),
            ..snapshot.clone()
        };
        let wire_other = serde_json::to_value(&other).expect("snapshot serializes");

        // Fold kernel rows through the live projection into extension state —
        // the exact path a reconnecting client's baseline takes: one
        // projector across both rows, the fold consuming each emitted action.
        let mut state = ext::XManoxState::default();
        let mut translator = Translator::new();
        for (seq, row) in [wire.clone(), wire_other].into_iter().enumerate() {
            for emitted in translator.on_entry(
                "c-1",
                "s-1",
                seq as u64,
                &kernel(json!({"type": "background_task", "snapshot": row})),
            ) {
                if emitted.channel == "x-manox-work:/s-1" {
                    let action = serde_json::to_value(&emitted.action).expect("action serializes");
                    assert_eq!(
                        ext::reducer::apply(&mut state, &action),
                        ext::ExtOutcome::Applied
                    );
                }
            }
        }
        assert_eq!(
            state.background_tasks.as_ref().and_then(|m| m.get("mon_7")),
            Some(&wire),
            "the host shape reaches the registry view verbatim, keyed by task id"
        );

        // The registry property the whole-field fold used to break: a second
        // task's row joins the view instead of replacing it, so a
        // reconnecting client's baseline carries every task the journal has
        // rows for.
        assert_eq!(
            state
                .background_tasks
                .expect("registry view populated")
                .len(),
            2,
            "each task keeps its own registry entry"
        );
    }

    /// The fold's determinism gate: the same records projected twice yield
    /// byte-identical action streams, and a projector seeded from the folded
    /// state (the restart path) continues the same run numbering.
    #[test]
    fn replaying_the_same_records_yields_identical_actions() {
        let records = vec![
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "message",
                "id": "e-2",
                "message": {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                "origin": "rpc-1",
            })),
            kernel(json!({"type": "agent_text_delta", "id": "e-3", "delta": "hi"})),
            kernel(json!({"type": "tool_call", "id": "e-4", "callId": "t-1",
                          "name": "bash", "title": "run", "status": "running", "input": {}})),
            kernel(json!({"type": "turn_finish", "id": "e-5",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
        ];
        let project = |records: &[SessionTreeEntry]| {
            let mut translator = Translator::new();
            records
                .iter()
                .enumerate()
                .flat_map(|(seq, entry)| translator.on_entry("c-1", "s-1", seq as u64, entry))
                .map(|emitted| (emitted.channel.clone(), emitted.action.clone()))
                .collect::<Vec<_>>()
        };
        let first = project(&records);
        let second = project(&records);
        assert_eq!(first, second, "replay is deterministic");
        assert!(
            first.iter().any(|(channel, _)| channel == "ahp-chat:/c-1"),
            "the stream reaches the chat channel: {first:?}"
        );
    }
}

#[cfg(test)]
mod turn_message_tests {
    use super::*;

    #[test]
    fn the_initiating_user_row_becomes_the_turns_opening_message() {
        let mut t = Translator::new();
        let mut out = Vec::new();
        for (seq, entry) in [
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "message",
                "id": "e-2",
                "message": {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                "origin": "rpc-1",
            })),
            kernel(json!({"type": "agent_text_delta", "id": "e-3", "delta": "hi"})),
        ]
        .into_iter()
        .enumerate()
        {
            out.extend(t.on_entry("c-1", "s-1", seq as u64, &entry));
        }
        let started = out.iter().find(
            |e| matches!(&e.action, StateAction::ChatTurnStarted(a) if a.message.text == "hello"),
        );
        assert!(started.is_some(), "turnStarted with the user text: {out:?}");
        assert!(
            !out.iter().any(|e| matches!(
                &e.action,
                StateAction::ChatPendingMessageSet(p)
                    if p.kind == PendingMessageKind::Steering
            )),
            "the initiating row is not a steer: {out:?}"
        );
    }

    #[test]
    fn the_turn_started_meta_stays_owner_free() {
        // The open-turn owner is delivered on the `x-manox/openTurn` query
        // face (the fold and the live bridge never replay turnStarted meta —
        // an attach-time reader could not reach it there), so the meta stays
        // the plain entry id regardless of the row's owner stamp.
        let mut t = Translator::new();
        let mut out = Vec::new();
        out.extend(t.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({"type": "turn_start", "owner": {"pid": 4242}})),
        ));
        let meta = out
            .iter()
            .find_map(|e| match &e.action {
                StateAction::ChatTurnStarted(a) => a.meta.as_ref(),
                _ => None,
            })
            .expect("turnStarted carries meta");
        assert_eq!(meta["x-manox"]["entryId"], serde_json::json!("e-1"));
        assert!(
            meta["x-manox"].get("turnOwner").is_none(),
            "the meta must not carry the owner: {meta:?}"
        );
    }
}

/// The transcript-semantics gates, ported from the retired
/// `translation_convergence` suite when the wire vocabulary was deleted:
/// the same `on_message` state machine, now driven by kernel records. Each
/// one pins a misclassification a happy-path fold cannot catch — a steer
/// queued instead of injected runs its text twice, a stop row closed
/// mid-loop splits one turn per tool round, a textless ask strands the
/// model waiting forever.
#[cfg(test)]
mod fold_semantics_tests {
    use super::*;

    /// A turn, a mid-run steer under the client's steer id, and the turn's
    /// close — the rows a live steer produces on the kernel journal.
    fn mid_run_steer_records() -> Vec<SessionTreeEntry> {
        vec![
            kernel(json!({
                "type": "message",
                "id": "sentry-0",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "run the tests"}]},
                "origin": "rpc-submit",
            })),
            kernel(json!({"type": "turn_start", "id": "sentry-1"})),
            kernel(json!({"type": "agent_text_delta", "id": "sentry-2", "delta": "Starting."})),
            // The steer: a user row that arrives with the turn still open.
            // The id is the client's steer id, which is also the id its
            // `chat/pendingMessageSet{kind: steering}` echo carried.
            kernel(json!({
                "type": "message",
                "id": "steer-1",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "use the fast suite"}]},
                "origin": "steer-1",
            })),
            kernel(json!({"type": "agent_text_delta", "id": "sentry-4",
                          "delta": "Using the fast suite."})),
            kernel(json!({"type": "turn_finish", "id": "sentry-5",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
        ]
    }

    /// A mid-run steer is `Steering`, not `Queued` — and it is not published
    /// twice.
    ///
    /// A manox steer is injected into the *running* turn and journalled as
    /// an ordinary user row; it does not close the turn and does not wait
    /// for the next one. AHP draws that distinction in
    /// `PendingMessageKind`: `Steering` lands in
    /// `ChatState.steeringMessage` (consumed by the turn it interrupts),
    /// `Queued` lands in `ChatState.queuedMessages` (carried into the
    /// *next* turn). Projecting a steer as `Queued` therefore does not
    /// merely mislabel it: the row is still sitting in the queue when the
    /// next turn starts, so the same text is injected a second time.
    #[test]
    fn a_mid_run_steer_is_steering_not_queued() {
        let mut translator = Translator::new();
        let mut seen = Vec::new();
        for (seq, entry) in mid_run_steer_records().into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                if let StateAction::ChatPendingMessageSet(set) = &emitted.action {
                    seen.push((format!("{:?}", set.kind), set.id.clone()));
                }
            }
        }
        // Two pending messages, and that is correct: the opening submission
        // is a genuine queue entry (the turn carries it as its first
        // message), while the steer is not. What must never happen is the
        // steer appearing as a *second* queued entry, or under a second id.
        let kinds: Vec<&str> = seen.iter().map(|(kind, _)| kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["Queued", "Steering"],
            "the submission queues; the mid-run steer is injected, not queued (got {seen:?})"
        );
        let (_, steer_id) = &seen[1];
        assert_eq!(
            steer_id, "steer-1",
            "the pending id must be the client's steer id — the identity its echo \
             and the host's `steer` intent both use; a projector-minted id can be \
             retired by neither"
        );
        // The id is unique: a pending message the client cannot match against
        // the one it already holds is what produced two entries for one steer.
        let ids: Vec<&str> = seen.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(
            ids.len(),
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            "pending ids must be distinct: {ids:?}"
        );
    }

    /// The actions the projector emits for the mid-run steer scenario, reduced.
    fn mid_run_steer_state() -> ahp_types::state::ChatState {
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        for (seq, entry) in mid_run_steer_records().into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        state
    }

    #[test]
    fn a_consumed_steer_leaves_no_queued_residue() {
        let state = mid_run_steer_state();
        let queued = state.queued_messages.unwrap_or_default();
        assert!(
            queued.is_empty(),
            "the steer must not remain queued: a queued entry is re-injected as a \
             fresh user message by the next `turnStarted`, running the same text \
             twice (residue: {queued:?})"
        );
    }

    /// A later turn does not inherit the steer.
    ///
    /// This is the consequence the kind fix exists for. The projector's
    /// contract ends at "the steer is `Steering`, not `Queued`": it
    /// publishes no removal, because the removal belongs to whoever observes
    /// the injection. What the fold must show is that nothing carries the
    /// text forward.
    ///
    /// The removal itself is asserted where it is emitted, on the host path
    /// (the dispatch tests in `manox-session-core`), since this test drives
    /// the projector alone and so never sees it.
    #[test]
    fn a_steer_is_not_replayed_by_a_later_turn() {
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        for (seq, entry) in mid_run_steer_records().into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }

        // The steer reached the running turn, so the turn's opening message
        // is still the original submission — not the steer.
        let turn = state
            .turns
            .last()
            .expect("the scripted records open a turn");
        assert_eq!(
            turn.message.text, "run the tests",
            "the steer is injected into the running turn; it must not replace or \
             reopen it"
        );

        // Now the next turn starts. Nothing may carry the steer forward.
        let next = kernel(json!({"type": "turn_start", "id": "sentry-99"}));
        for emitted in translator.on_entry("c-1", "s-1", 99, &next) {
            ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
        }
        let queued = state.queued_messages.clone().unwrap_or_default();
        assert!(
            !queued.iter().any(|m| m.id == "steer-1"),
            "the next turn must not inherit the steer: {queued:?}"
        );
        // The steering slot itself is retired by the host's removal, not by
        // the fold — see the dispatch tests. Assert it is still the CLIENT's
        // id here, so that removal has something to match.
        assert_eq!(
            state.steering_message.as_ref().map(|m| m.id.as_str()),
            Some("steer-1"),
            "the pending steering entry keeps the client's id, which is what the \
             host's removal matches on"
        );
    }

    /// An agentic tool loop: every assistant message is followed by a `stop`
    /// record carrying the model's stop reason, and the turn only really
    /// ends at its `turn_finish`. The stops are per-message metadata — no
    /// notice may surface and no turn may close until the finish arrives.
    #[test]
    fn a_stop_row_is_metadata_and_never_ends_the_turn() {
        let records = [
            kernel(json!({
                "type": "message",
                "id": "sentry-0",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "look around"}]},
                "origin": "rpc-1",
            })),
            kernel(json!({"type": "agent_text_delta", "id": "sentry-1", "delta": "Working."})),
            kernel(json!({"type": "stop", "id": "sentry-2", "reason": "tool_use"})),
            kernel(json!({"type": "agent_text_delta", "id": "sentry-3",
                          "delta": "Still working."})),
            kernel(json!({"type": "stop", "id": "sentry-4", "reason": "end_turn"})),
            kernel(json!({"type": "turn_finish", "id": "sentry-5",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
        ];

        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        let mut stop_notes = 0usize;
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                if let StateAction::ChatResponsePart(part) = &emitted.action
                    && matches!(
                        part.part,
                        ahp_types::state::ResponsePart::SystemNotification(_)
                    )
                {
                    stop_notes += 1;
                }
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(
            stop_notes, 0,
            "a stop row is the model's own metadata, not a notice for the user"
        );
        assert_eq!(
            state.turns.len(),
            1,
            "the tool loop is one turn — the stops must not close it mid-round"
        );
        assert_eq!(
            state.turns.last().map(|t| t.message.text.as_str()),
            Some("look around"),
            "the turn's opening message is the submission"
        );
        assert!(
            state.active_turn.is_none(),
            "the turnFinish closed the turn"
        );
    }

    /// Records written by older engine builds state the AskUserQuestion
    /// text in `header` alone — the `question` field is a later schema
    /// addition (a device journal carries exactly this shape). The
    /// elicitation must still carry the structured questions; an
    /// unanswerable bare ask leaves the model waiting forever.
    #[test]
    fn an_older_build_ask_payload_folds_its_questions_through_the_header() {
        let entry = kernel(json!({
            "type": "question",
            "id": "e7",
            "kind": "request",
            "authId": "call_00_LZg8",
            "payload": {
                "toolName": "AskUserQuestion",
                "toolCallId": "call_00_LZg8",
                "input": {"questions": [{
                    "header": "交付方式",
                    "multiSelect": false,
                    "options": [
                        {"label": "发到 PR 评论区", "description": "gh pr comment"},
                        {"label": "贴回对话", "description": "直接回复"},
                    ],
                }]},
            },
        }));
        let mut translator = Translator::new();
        let actions: Vec<_> = translator
            .on_entry("c-1", "s-1", 7, &entry)
            .into_iter()
            .map(|e| e.action)
            .collect();
        let request = actions.iter().find_map(|a| match a {
            StateAction::ChatInputRequested(r) => Some(&r.request),
            _ => None,
        });
        let request = request.expect("the ask reaches the client as chat/inputRequested");
        assert_eq!(request.id, "call_00_LZg8");
        let questions = request.questions.as_ref().expect("structured questions");
        assert_eq!(
            questions.len(),
            1,
            "the v1 payload still yields its question"
        );
        let single = match &questions[0] {
            ahp_types::state::ChatInputQuestion::SingleSelect(s) => s,
            other => panic!("expected a single-select question, got {other:?}"),
        };
        assert_eq!(
            single.message, "交付方式",
            "the header IS the question text"
        );
        assert_eq!(single.options.len(), 2, "both options survive");
    }

    /// A question row carrying neither `question` nor `header` has nothing
    /// to ask: the fold must degrade to the accepted bare ask
    /// (`questions: None`), never to a silent textless card.
    #[test]
    fn an_ask_row_without_any_question_text_degrades_to_a_bare_ask() {
        let entry = kernel(json!({
            "type": "question",
            "id": "e8",
            "kind": "request",
            "authId": "call_bare",
            "payload": {
                "toolName": "AskUserQuestion",
                "input": {"questions": [{"multiSelect": false, "options": []}]},
            },
        }));
        let mut translator = Translator::new();
        let actions: Vec<_> = translator
            .on_entry("c-1", "s-1", 8, &entry)
            .into_iter()
            .map(|e| e.action)
            .collect();
        let request = actions.iter().find_map(|a| match a {
            StateAction::ChatInputRequested(r) => Some(&r.request),
            _ => None,
        });
        let request = request.expect("the bare ask still reaches the client");
        assert_eq!(
            request.questions.as_ref(),
            None,
            "no text means no fabricated question card"
        );
    }
}

/// The extension-state reachability gate: every `XManoxState` field must be
/// fillable through the real production path — a kernel record projected by
/// [`Translator`], riding the exact extension channel its fold lives on.
/// Ported from `manox_ahp::ext::reducer`'s tests when the translator moved
/// here (the ext crate cannot name the runtime's projector). The compile-time
/// half is the struct literal at the bottom — no `..Default`, so a new
/// `XManoxState` field without a producer case above is a missing literal
/// field, and "declared but always empty" cannot come back.
#[cfg(test)]
mod extension_gate_tests {
    use super::*;
    use manox_ahp::ext::ExtOutcome;
    use manox_ahp::ext::XManoxState;
    use manox_ahp::ext::reducer as ext_reducer;

    #[test]
    fn every_extension_state_field_is_reachable_through_the_real_fold() {
        use manox_ahp::ext::actions as ext_actions;
        // Host-emitted: kernel record → the tag it must produce and the exact
        // channel it must ride (an action on the WRONG extension channel
        // still folds — into the wrong bag — so `is_extension_channel` alone
        // cannot catch it).
        let host_cases: Vec<(SessionTreeEntry, &str, &str)> = vec![
            (
                kernel(json!({"type": "plan_mode_change", "enabled": true})),
                ext_actions::PLAN_MODE_CHANGED,
                "x-manox-plan:/c-1",
            ),
            (
                kernel(json!({"type": "plan_update", "snapshot": {}})),
                ext_actions::PLAN_CHANGED,
                "x-manox-plan:/c-1",
            ),
            (
                kernel(json!({"type": "plan_review", "state": "proposed",
                              "requestId": "plan-review:e-gate"})),
                ext_actions::PLAN_VERDICT_REQUESTED,
                "x-manox-plan:/c-1",
            ),
            (
                kernel(json!({"type": "goal", "goal": {"text": "ship"}})),
                ext_actions::WORK_GOAL_CHANGED,
                "x-manox-work:/s-1",
            ),
            (
                kernel(json!({"type": "browser_suites", "suites": ["chrome"]})),
                ext_actions::WORK_BROWSER_SUITES,
                "x-manox-work:/s-1",
            ),
            (
                kernel(json!({"type": "background_task", "snapshot": {"task_id": "mon_7"}})),
                ext_actions::WORK_BACKGROUND_TASKS,
                "x-manox-work:/s-1",
            ),
            (
                kernel(json!({"type": "subagent_progress", "agentId": "sub-0",
                              "agentType": "explore", "toolUses": 0,
                              "latestActivity": null, "status": "running"})),
                ext_actions::WORK_SUBAGENTS,
                "x-manox-work:/s-1",
            ),
            (
                kernel(json!({"type": "active_tools_change", "activeToolNames": ["bash"]})),
                ext_actions::WORK_ACTIVE_TOOLS,
                "x-manox-work:/s-1",
            ),
            (
                kernel(json!({"type": "metrics", "metricType": "token_usage", "data": {}})),
                ext_actions::METRICS_CHANGED,
                "x-manox-metrics:/c-1",
            ),
            (
                kernel(json!({"type": "pinned_archived", "pinned": true, "archived": false})),
                ext_actions::PINNED_CHANGED,
                "x-manox-thread:/s-1",
            ),
            (
                kernel(json!({"type": "label", "targetId": "e-gate", "label": "x"})),
                ext_actions::LABEL_CHANGED,
                "x-manox-thread:/s-1",
            ),
            (
                kernel(json!({"type": "session_info", "name": "agent"})),
                ext_actions::SESSION_INFO_CHANGED,
                "x-manox-thread:/s-1",
            ),
            (
                kernel(json!({"type": "leaf", "targetId": "e-9"})),
                ext_actions::LEAF_CHANGED,
                "x-manox-thread:/s-1",
            ),
        ];
        // Client-dispatched: the action as the client sends it (no journal
        // producer exists; the runtime dispatch arms accept these directly).
        let client_cases: Vec<(Value, &str)> = vec![(
            json!({"type": ext_actions::ORDER_CHANGED, "order": {"s-1": 1}}),
            ext_actions::ORDER_CHANGED,
        )];
        // Excluded, no fold arm to feed: BASELINE (host envelope),
        // PLAN_REVIEW_SETTLED (folds with the host-emitted verdict-requested edge),
        // the workspaces rows (declared, fold lives outside this bag).

        let mut state = XManoxState::default();
        let mut plan_review_action = None;
        let mut translator = Translator::new();
        for (entry, tag, expected_channel) in &host_cases {
            assert!(ext_actions::ALL.contains(tag), "{tag} must stay declared");
            let emitted = translator.on_entry("c-1", "s-1", 0, entry);
            let hit = emitted
                .iter()
                .find(|e| serde_json::to_value(&e.action).unwrap()["type"] == *tag)
                .unwrap_or_else(|| panic!("{tag} has no projector producer"));
            let action = serde_json::to_value(&hit.action).unwrap();
            assert!(
                manox_ahp::ext::is_extension_channel(&hit.channel),
                "{tag} must ride an extension channel; it was emitted on {} where no fold runs",
                hit.channel
            );
            assert_eq!(
                hit.channel, *expected_channel,
                "{tag} was emitted on the wrong extension channel; its fold would land in \
                 another channel's bag and the expected channel's baseline would stay empty"
            );
            assert_eq!(
                ext_reducer::apply(&mut state, &action),
                ExtOutcome::Applied,
                "{tag} did not fold: {action}"
            );
            if *tag == ext_actions::PLAN_VERDICT_REQUESTED {
                plan_review_action = Some(action.clone());
            }
        }
        for (action, tag) in &client_cases {
            assert_eq!(
                ext_reducer::apply(&mut state, action),
                ExtOutcome::Applied,
                "{tag} did not fold: {action}"
            );
        }

        // Total coverage: every field of the bag, populated by the cases
        // above. The struct literal has no `..Default` on purpose.
        assert_eq!(
            state,
            XManoxState {
                plan_mode: Some(true),
                plan: Some(json!({})),
                plan_review: plan_review_action,
                goal: Some(json!({"text": "ship"})),
                browser_suites: Some(vec!["chrome".to_string()]),
                background_tasks: Some(
                    [("mon_7".to_string(), json!({"task_id": "mon_7"}))]
                        .into_iter()
                        .collect()
                ),
                subagents: Some(
                    [(
                        "sub-0".to_string(),
                        json!({
                            "agentId": "sub-0", "agentType": "explore",
                            "toolUses": 0, "latestActivity": null,
                            "status": "running"
                        }),
                    )]
                    .into_iter()
                    .collect()
                ),
                pinned: Some(true),
                label: Some("x".to_string()),
                session_info: Some(json!({"name": "agent"})),
                leaf: Some("e-9".to_string()),
                active_tools: Some(vec!["bash".to_string()]),
                order: Some(json!({"s-1": 1})),
            },
            "every extension-state field must be reachable from a live producer"
        );
    }
}
