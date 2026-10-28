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
    ConfirmationOption, ConfirmationOptionKind, ContentRef, ErrorInfo, ErrorResponsePart, FileEdit,
    FileEditCollection, FileEditDiffStats, FileEditSide, MarkdownResponsePart, Message,
    MessageAttachment, MessageEmbeddedResourceAttachment, MessageKind, MessageOrigin,
    PendingMessageKind, ReasoningResponsePart, ResponsePart, SessionInputRequest,
    SessionToolConfirmationRequest, SystemNotificationResponsePart, ToolCallCancellationReason,
    ToolCallConfirmationReason, ToolCallConfirmationState, ToolCallPendingConfirmationState,
    ToolCallResult, ToolCallState, ToolInput, ToolResultContent, ToolResultTextContent, UsageInfo,
};
use manox_harness::session::SessionTreeEntry;
use manox_harness::types::{AgentMessage, ContentBlock};
use serde_json::{Value, json};

use manox_ahp::ext;

/// Session config value keys this translator writes.
///
/// The runtime's `ConfigSchema` declares the same keys: this is the naming
/// authority for the values that ride `session/configChanged` — the spec's config
/// model (model / effort / access mode) plus the session facts AHP has no field
/// for.
pub mod config_keys {
    /// Canonical `provider/model` reference (L8).
    pub const MODEL: &str = "model";
    /// Reasoning-effort vocabulary string.
    pub const REASONING_EFFORT: &str = "reasoningEffort";
    /// Access-mode vocabulary string.
    pub const ACCESS_MODE: &str = "accessMode";
    /// Network-access vocabulary string (`off`/`on`) for sandboxed bash.
    pub const NETWORK_ACCESS: &str = "networkAccess";
    /// Server-owned project path (`null` clears).
    pub const PROJECT: &str = "project";
    /// Effective working directory of this journal.
    pub const WORKING_DIRECTORY: &str = "workingDirectory";
    /// The keys a client may change mid-session through
    /// `session/configChanged` — exactly the set the declared schema marks
    /// `sessionMutable: true` (AHP: only session-mutable properties may be
    /// changed). The server-owned keys (`project`, `workingDirectory`)
    /// travel this seat host→client only: `workingDirectory` writes ride
    /// the dedicated `session/workingDirectorySet` action, so their arrival
    /// through a config dispatch is a client bug, not a fold.
    pub const MUTABLE: &[&str] = &[MODEL, REASONING_EFFORT, ACCESS_MODE, NETWORK_ACCESS];
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
    /// `x-manox-plan:/<chat-id>` — the execution-plan document.
    Plan,
    /// `x-manox-work:/<session-id>` — goal, background work, sub-agents, suites.
    Work,
    /// `x-manox-metrics:/<chat-id>` — aggregated conversation metrics.
    Metrics,
    /// `ahp-session:/<session-id>/annotations` — durable user annotations.
    Annotations,
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
            Self::Annotations => manox_ahp::channels::annotations::uri(session_id),
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
        // Swarm lifecycle and disposal are chat-level facts (background work
        // plus settlement notices); disposal itself is a session-level fact.
        E::RunStarted { .. }
        | E::RunSettled { .. }
        | E::RunEscalated { .. }
        | E::RunContinued { .. } => Target::Chat,
        // ── session state ────────────────────────────────────────────
        E::ModelChange { .. }
        | E::CwdChange { .. }
        | E::ProjectChange { .. }
        | E::ThinkingLevelChange { .. }
        | E::AccessModeChange { .. }
        | E::Title { .. }
        // `PinnedArchived` lands here for its native archived half; the
        // extension pin half re-targets the thread channel at its emit site.
        | E::PinnedArchived { .. } => Target::Session,
        // Thread rows: the extension face is their only face — on the session
        // channel no fold ever consumed them (see [`Target::Thread`]).
        E::Label { .. } | E::SessionInfo { .. } | E::Leaf { .. } => Target::Thread,
        // Annotation rows land here for the durable face; a UI note card's
        // transcript half re-targets the chat channel at its emit site.
        E::AnnotationSet { .. } | E::AnnotationRemoved { .. } => Target::Annotations,
        // ── plan (the execution-plan snapshot channel) ────────────────
        E::PlanUpdate { .. } => Target::Plan,
        // ── extension work surface ───────────────────────────────────
        E::Goal { .. } | E::BrowserSuites { .. } | E::BackgroundTask { .. } => Target::Work,
        E::ActiveToolsChange { .. } => Target::Work,
        // ── approvals / questions ride the chat's tool-call + input state ──
        E::Approval { .. } | E::Question { .. } => Target::Chat,
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
    /// The call's journaled parameters (latest row wins) — the source the
    /// edit-preview face is derived from when the approval gate opens, and
    /// the payload every `toolInput`-bearing emission forwards.
    input: Option<Value>,
    /// The parameters have reached the wire on a `ChatToolCallReady` (the
    /// only action whose reducer lands `toolInput` on every state after
    /// `streaming`). A `chat/toolCallDelta` parks its content on the
    /// streaming part's `partialInput`, which the ready transition drops —
    /// so this flag, not the delta, is what keeps the parameters from being
    /// published twice.
    input_published: bool,
    /// A `chat/toolCallStart` created the part.
    started: bool,
    /// The part sits in `pending-confirmation`.
    awaiting: bool,
    /// The part is `running`.
    working: bool,
    /// A terminal action settled the part (`completed` or `cancelled`).
    settled: bool,
    /// The gate's `authId` while the part is parked in `pending-confirmation`:
    /// the identity a refreshed session mirror must carry.
    auth_id: Option<String>,
    /// The confirmation text the gate opened with. A ready action restates the
    /// invocation message rather than leaving it in place, so the catch-up
    /// republishes this exact prompt instead of rebuilding it from a title the
    /// approval-first order has not learned yet.
    pending_prompt: Option<String>,
    /// The confirmation reason the current running state was published with
    /// (`UserAction` after an approval, `NotNeeded` otherwise). Every ready
    /// action rewrites the field, so a later parameter publication restates
    /// this rather than flattening an explicit approval to `NotNeeded`.
    confirmed: Option<ToolCallConfirmationReason>,
    /// Live output accumulated for `chat/toolCallContentChanged`, which replaces
    /// the content array rather than appending to it.
    output: String,
}

impl CallBook {
    fn new(tool_name: &str, title: &str) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            title: title.to_string(),
            input: None,
            input_published: false,
            started: false,
            awaiting: false,
            working: false,
            settled: false,
            auth_id: None,
            pending_prompt: None,
            confirmed: None,
            output: String::new(),
        }
    }

    /// Adopt the state a seeded (already folded) tool call shows.
    fn adopt(title: &str, state: &ToolCallState) -> Self {
        let mut book = Self::new("", title);
        // A seeded call's parameters are not recoverable from the folded
        // state, so the preview face is simply absent across a restart
        // mid-gate — the next gate re-derives it from its own row.
        book.started = true;
        match state {
            ToolCallState::Streaming(_) => {}
            ToolCallState::PendingConfirmation(_) => book.awaiting = true,
            ToolCallState::Running(s) => {
                book.working = true;
                book.confirmed = Some(s.confirmed.clone());
            }
            ToolCallState::AuthRequired(s) => {
                book.working = true;
                book.confirmed = Some(s.confirmed.clone());
            }
            ToolCallState::PendingResultConfirmation(s) => {
                book.working = true;
                book.confirmed = Some(s.confirmed.clone());
            }
            ToolCallState::Completed(state) => {
                book.working = true;
                book.settled = true;
                book.confirmed = Some(state.confirmed.clone());
            }
            ToolCallState::Cancelled(_) => {
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
    /// A text delta streamed since the last settled assistant row: the row
    /// stays silent about text (first-writer-wins).
    streamed_text: bool,
    /// A thinking delta streamed since the last settled assistant row: like
    /// [`OpenTurn::streamed_text`], it marks the row as the non-first writer
    /// of a streamed reply, whose tail deltas must rejoin the runs they
    /// streamed into instead of opening fresh parts.
    streamed_thinking: bool,
    /// The opening user message the turn carries: the `turnStart`'s carried
    /// pending user when there was one, else empty until the initiating user
    /// row lands. Recording the carried text is what keeps a later pre-content
    /// user row (a parked steer consumed at run start) from reading as "no
    /// opening yet" and replacing it.
    message_text: String,
    /// Tool calls by handle, in a sorted map so turn-boundary settlement is
    /// deterministic.
    calls: BTreeMap<String, CallBook>,
    /// Elicitations requested on this turn and not yet decided, in journal
    /// order (the auth id is the request id). The gate journals a
    /// `cancelled` decision when a turn dies with a parked card, so the
    /// normal replay settles the part before this list is consulted; the
    /// remainder here is the crash-before-discard residue that
    /// [`Translator::finish_turn`] settles at the boundary.
    open_asks: Vec<String>,
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
            streamed_thinking: false,
            calls: BTreeMap::new(),
            open_asks: Vec::new(),
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
    /// Subagent run ids currently listed in the parent chat's background
    /// work, so a progress tick settles to exactly one Set edge per run and
    /// exactly one Removed edge at its terminal status.
    subagents: BTreeMap<String, ()>,
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
    verdict: Option<&'a str>,
    reason: Option<&'a str>,
    input: Option<&'a serde_json::Value>,
}

/// The ask's support markdown for the request-level `message` field: the
/// question-level `detail` payloads, joined under their headers when more
/// than one carries content. The AHP wire has no per-question detail slot,
/// so this is the only route support text reaches a client.
fn ask_details_markdown(input: &serde_json::Value) -> Option<String> {
    let questions = input.get("questions")?.as_array()?;
    let details: Vec<(&str, &str)> = questions
        .iter()
        .filter_map(|q| {
            let detail = q.get("detail").and_then(|v| v.as_str()).unwrap_or("");
            (!detail.trim().is_empty()).then(|| {
                let header = q.get("header").and_then(|v| v.as_str()).unwrap_or("");
                (header, detail)
            })
        })
        .collect();
    match details.len() {
        0 => None,
        1 => Some(details[0].1.to_string()),
        _ => Some(
            details
                .iter()
                .map(|(header, detail)| {
                    if header.is_empty() {
                        (*detail).to_string()
                    } else {
                        format!("## {header}\n\n{detail}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n---\n\n"),
        ),
    }
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

    /// Backfill the subagent roster from the host's folded chat state.
    ///
    /// A bridge reset (restart or lag resync) installs a fresh translator whose
    /// roster is empty, while the client's fold keeps every background-work
    /// entry published before the reset — the `run/started` rows that opened
    /// them sit below the resume watermark and are never replayed. Primed ids
    /// make the later `run/settled` (and a cancelled turn's #812 sweep) see
    /// those entries as listed, so their removals go out instead of orphaning
    /// the client's rows. Bookkeeping only: no edge is published, the client
    /// already holds these entries.
    pub fn prime_subagent_roster(&mut self, ids: impl IntoIterator<Item = String>) {
        for id in ids {
            self.subagents.insert(id, ());
        }
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
                // A cancelled turn kills its in-flight subagents (#812) —
                // the AHP face reports the deaths as removals rather than
                // leaving entries that never settle. A completed turn's runs
                // settle through their own terminal rows.
                if *cancelled {
                    for id in self.subagents.keys().cloned().collect::<Vec<_>>() {
                        self.subagents.remove(&id);
                        out.push(Emitted::new(
                            &chat,
                            StateAction::ChatBackgroundWorkRemoved(
                                ahp_types::actions::ChatBackgroundWorkRemovedAction { id },
                            ),
                        ));
                    }
                }
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
                self.on_tool_call(
                    &chat, &session, &row, call_id, name, title, status, &input, &mut out,
                );
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
            SessionTreeEntry::RunStarted {
                run_id,
                kind,
                label,
                tool_call_id,
                ..
            } => {
                // The durable lifecycle rows are the whole background-work
                // truth: one open at start, one close at settle — and the
                // child's own state lives in its chat (`ahp-chat:/<run_id>`;
                // the run id names the child session). No progress rides
                // this pointer.
                let listed = self.subagents.contains_key(run_id);
                if !listed {
                    self.subagents.insert(run_id.clone(), ());
                    out.push(Emitted::new(
                        &chat,
                        StateAction::ChatBackgroundWorkSet(
                            ahp_types::actions::ChatBackgroundWorkSetAction {
                                work: ahp_types::state::BackgroundWork::Subagent(
                                    ahp_types::state::BackgroundSubagentWork {
                                        id: run_id.clone(),
                                        label: label.clone(),
                                        started_at: row.timestamp.clone(),
                                        meta: None,
                                        chat: format!("ahp-chat:/{run_id}"),
                                    },
                                ),
                            },
                        ),
                    ));
                }
                // The spawning tool call's forward pointer — the same edge
                // the child chat records in reverse through its ChatOrigin
                // (the spec's MUST-keep-consistent pair). The tool call id
                // is the join key the row has carried since it rode the
                // start request.
                if !tool_call_id.is_empty() {
                    self.ensure_turn(&row, &chat, &mut out);
                    let turn_id = self.turn_id();
                    out.push(Emitted::new(
                        &chat,
                        StateAction::ChatToolCallContentChanged(
                            ahp_types::actions::ChatToolCallContentChangedAction {
                                turn_id,
                                tool_call_id: tool_call_id.clone(),
                                meta: None,
                                content: vec![ahp_types::state::ToolResultContent::Subagent(
                                    ahp_types::state::ToolResultSubagentContent {
                                        resource: format!("ahp-chat:/{run_id}"),
                                        title: label.clone(),
                                        agent_name: Some(kind.clone()),
                                        description: Some(label.clone()),
                                    },
                                )],
                            },
                        ),
                    ));
                }
            }
            SessionTreeEntry::RunSettled {
                run_id,
                verdict,
                salvage,
                turns,
                tool_calls,
                ..
            } => {
                let listed = self.subagents.contains_key(run_id);
                if listed {
                    self.subagents.remove(run_id);
                    out.push(Emitted::new(
                        &chat,
                        StateAction::ChatBackgroundWorkRemoved(
                            ahp_types::actions::ChatBackgroundWorkRemovedAction {
                                id: run_id.clone(),
                            },
                        ),
                    ));
                }
                // The settlement notice rides the standard face: the host's
                // verdict, the salvaged output or the explicit line that
                // there is none, and the counters. Visible to the agent and
                // the user alike — a settlement is never silent.
                let mut text = verdict.clone();
                match salvage {
                    Some(salvage) if !salvage.trim().is_empty() => {
                        text.push_str("\n\n");
                        text.push_str(salvage);
                    }
                    _ => text.push_str(" It left no closing message."),
                }
                text.push_str(&format!("\n\n({turns} turns, {tool_calls} tool calls)"));
                self.push_note(
                    &chat,
                    &row,
                    text,
                    serde_json::json!({ "runSettled": { "runId": run_id } }),
                    &mut out,
                );
            }
            SessionTreeEntry::RunContinued {
                run_id, follow_up, ..
            } => {
                // A continuation re-opens the run's roster entry: the same
                // child chat, a fresh start line, every pointer intact.
                let listed = self.subagents.contains_key(run_id);
                if !listed {
                    self.subagents.insert(run_id.clone(), ());
                    out.push(Emitted::new(
                        &chat,
                        StateAction::ChatBackgroundWorkSet(
                            ahp_types::actions::ChatBackgroundWorkSetAction {
                                work: ahp_types::state::BackgroundWork::Subagent(
                                    ahp_types::state::BackgroundSubagentWork {
                                        id: run_id.clone(),
                                        // The follow-up is the live topic
                                        // of the reopened run.
                                        label: follow_up.clone(),
                                        started_at: row.timestamp.clone(),
                                        meta: None,
                                        chat: format!("ahp-chat:/{run_id}"),
                                    },
                                ),
                            },
                        ),
                    ));
                }
            }
            SessionTreeEntry::RunEscalated {
                run_id, question, ..
            } => {
                // The child's clarifying question, attributed and visible to
                // the agent and the user alike: the note names the run and
                // coaches the continuation contract (the answer re-dispatches
                // with `continue_run_id`; the child session keeps its
                // context).
                let text = format!(
                    "Sub-agent `{run_id}` asks: {question}\n\nAnswer the user, then \
                     continue the run with `continue_run_id = \"{run_id}\"` carrying the \
                     answer as the prompt."
                );
                self.push_note(
                    &chat,
                    &row,
                    text,
                    serde_json::json!({ "runEscalated": { "runId": run_id } }),
                    &mut out,
                );
            }
            SessionTreeEntry::SubagentProgress { .. } => {
                // Retired forwarding rows: run/* rows are the only
                // background-work truth (no `_meta` payload, no per-tick
                // refresh). Old journals still parse; they fold to nothing.
            }
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
            SessionTreeEntry::AccessModeChange { mode, .. } => out.push(Emitted::new(
                &session,
                config_changed(json!({config_keys::ACCESS_MODE: mode})),
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
            // ── annotations ──────────────────────────────────────────────
            SessionTreeEntry::AnnotationSet { annotation, .. } => {
                // The row rides the AHP `Annotation` JSON verbatim; a row that
                // does not parse as one is skipped loudly — the durable state
                // stays as the last valid row left it.
                match serde_json::from_value::<ahp_types::state::Annotation>(annotation.clone()) {
                    Ok(annotation) => {
                        out.push(Emitted::new(
                            &at,
                            StateAction::AnnotationsSet(ahp_types::actions::AnnotationsSetAction {
                                annotation: annotation.clone(),
                            }),
                        ));
                        // A UI note card (an annotation stamped with the
                        // `x-manox` note payload) also rides the chat face, so
                        // a replayed transcript interleaves it in journal order
                        // instead of clients tail-appending it from the
                        // annotations channel. Plain review annotations have no
                        // transcript face.
                        if let Some((text, payload)) = annotation_note_face(&annotation) {
                            self.push_note(&chat, &row, text, payload, &mut out);
                        }
                    }
                    Err(error) => tracing::warn!(
                        seq = row.seq,
                        %error,
                        "annotation row does not parse as an AHP annotation; skipped"
                    ),
                }
            }
            SessionTreeEntry::AnnotationRemoved { annotation_id, .. } => out.push(Emitted::new(
                &at,
                StateAction::AnnotationsRemoved(ahp_types::actions::AnnotationsRemovedAction {
                    annotation_id: annotation_id.clone(),
                }),
            )),
            // ── plan (execution snapshot) ──────────────────────────────
            SessionTreeEntry::PlanUpdate { snapshot, .. } => out.push(Emitted::new(
                &at,
                extension_action(ext::actions::PLAN_CHANGED, json!({"snapshot": snapshot})),
            )),
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
                // A `user` row means one of three things:
                //
                // - **No turn is open** — an ordinary queued submission, which
                //   the next `turn_start` carries as its opening message.
                // - **A turn is open with nothing in it yet** — the initiating
                //   row of that turn (journal order puts `turnStart` before the
                //   user row). Re-issue the `turnStarted` with the text; the
                //   reducer replaces the still-empty active turn and no parts
                //   exist to orphan.
                // - **A turn is open and already carries content** — a manox
                //   *steer* injected into the running turn. It partitions the
                //   transcript instead of filling a pending slot; see below.
                let initiating = self.open.as_ref().is_some_and(|turn| {
                    turn.message_text.is_empty()
                        && !turn.streamed_text
                        && turn.text_part.is_none()
                        && turn.calls.is_empty()
                });
                let steering = self.open.is_some();
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
                if steering {
                    // A mid-run steer is its own transcript row. Folding it into
                    // a `Steering` pending slot kept the text outside the
                    // journal: the injection receipt cleared the slot live, a
                    // replay never did, and — either way — the reply to the
                    // steer streamed into the pre-steer turn, so journal order
                    // and reading order diverged. Partition instead: settle the
                    // running turn's content where it was produced and open the
                    // next turn on the steer row, so every fold (live or
                    // replayed from the journal) reads in journal order.
                    //
                    // A steer row lands at a kernel round boundary, never inside
                    // a tool call or a parked ask: the loop polls its steering
                    // queue at the top of each round (`agent_loop.rs`), after the
                    // round's tools have settled. `finish_turn`'s boundary
                    // settlement of open calls/asks is therefore defensive
                    // (a must-not-hang rule), not a reachable cancel of live
                    // work.
                    if let Some(open) = self.open.take() {
                        // The pre-steer round had completed its work; the steer
                        // continues the run in a new turn.
                        self.finish_turn(&open, chat, Outcome::Complete, entry, true, out);
                    }
                    let turn_id = format!("t-{}", entry.id);
                    // The row id IS the steer id: the kernel appends a
                    // caller-minted user message under that id (S3
                    // `user_entry_id`), and the client's
                    // `chat/pendingMessageSet{kind: steering}` used the same id —
                    // the host's `steer` intent keys the injected row by it. The
                    // row carries no `origin` (the origin pin belongs to the
                    // submit path), so `originRpc` cannot name the slot; a
                    // projector-minted `m-<id>` names nothing the client holds.
                    // Using the row id also covers non-client injections (a peer
                    // delivery's storage id matches no slot, which is harmless)
                    // and lets the reducer clear the steering slot a
                    // `turnStarted` names, so the slot's live lifetime is
                    // unchanged live and needs no removal on replay.
                    let queued_message_id = entry.id.clone();
                    out.push(Emitted::new(
                        chat,
                        StateAction::ChatTurnStarted(ChatTurnStartedAction {
                            turn_id: turn_id.clone(),
                            started_at: entry.timestamp.clone(),
                            message: message.clone(),
                            queued_message_id: Some(queued_message_id),
                            // Minted by the fold, not by a `turnStart` row: the
                            // same shape `ensure_turn` publishes.
                            meta: Some(manox_meta(json!({"synthetic": true, "entryId": entry.id}))),
                        }),
                    ));
                    let mut open = OpenTurn::new(turn_id, entry.timestamp.clone(), true);
                    open.message_text = message.text.clone();
                    self.open = Some(open);
                    return;
                }
                // No turn is open: a queued submission joins `pending_user`,
                // and the next `turn_start` drains it into its opening message.
                let id = format!("m-{}", entry.id);
                out.push(Emitted::new(
                    chat,
                    StateAction::ChatPendingMessageSet(ChatPendingMessageSetAction {
                        kind: PendingMessageKind::Queued,
                        id: id.clone(),
                        message: message.clone(),
                    }),
                ));
                self.pending_user.push_back(PendingUser { id, message });
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
                    // A run streamed on this reply keeps its slot across the
                    // row: the journal's write paths race (deltas ride the
                    // notice tap's actor queue, the settled row is written
                    // directly), so a reply's tail deltas can land after the
                    // row that already carries their text. Those deltas are
                    // that reply's own suffix — rejoining them into the open
                    // part rebuilds exactly the settled text — while a delta
                    // opening a fresh part here would split one reply in two,
                    // and replay the split forever. The closers the fold owns
                    // (the other stream, a tool call, a notification, the turn
                    // boundary) still fire, so an open slot never reaches an
                    // unrelated response; a row that is itself the first
                    // writer closes the runs as before.
                    if !streamed && !turn.streamed_thinking {
                        turn.close_runs();
                    }
                    turn.streamed_text = false;
                    turn.streamed_thinking = false;
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
            self.finish_turn(&open, chat, Outcome::Complete, entry, false, out);
        }
        let carried = self.pending_user.pop_front();
        let (message, queued_message_id, opening_text) = match carried {
            Some(pending) => {
                let text = pending.message.text.clone();
                (pending.message, Some(pending.id), text)
            }
            None => (no_user_row(entry), None, String::new()),
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
        let mut open = OpenTurn::new(id, entry.timestamp.clone(), false);
        // Record the opening message the turn now carries. Without it a later
        // user row that arrives before any assistant content (a parked steer
        // consumed at run start) reads as "the turn has no opening yet" and
        // REPLACES the carried text — the prompt would vanish from the fold
        // while its journal row stays behind. With it recorded, such a row
        // partitions instead: the carried opening becomes its own (empty)
        // turn and the steer opens the next one.
        open.message_text = opening_text;
        self.open = Some(open);
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
                    turn.streamed_thinking = true;
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
        session: &str,
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
            if let Some(book) = turn.calls.get_mut(call_id) {
                // The approval-first order creates the book from the gate's
                // payload, which knows neither the row's title nor (when the
                // gate names no tool) its name. The first row that lands fills
                // the blanks; a later status row must not overwrite them with
                // its own generic title.
                if book.tool_name.is_empty() {
                    book.tool_name = name.to_string();
                }
                if book.title.is_empty() {
                    book.title = title.to_string();
                }
                // A status-update row repeating the published parameters keeps
                // the publication; a row whose parameters differ (and are not
                // absent) re-arms it so the catch-up can deliver them.
                if book.input_published
                    && book.input.as_ref().is_some_and(|prev| prev != input)
                    && !input.is_null()
                {
                    book.input_published = false;
                }
                book.input = Some(input.clone());
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
        // `streaming`. The durable publication is the phase transition below (or
        // the catch-up): the ready transition drops `partialInput`, so a ready
        // action is what must carry them.
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
        // A call the wire already moved past `streaming` (the gate opened before
        // this row, or an earlier phase transition published nothing — the
        // parameters were not yet journaled) takes them through the catch-up.
        if !first {
            self.publish_input(chat, session, &turn_id, call_id, out);
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
        let approved = matches!(
            verdict.unwrap_or_default(),
            "allow_once" | "allow_host_session" | "allow_host_persistent" | "answered"
        );
        let turn_id = match kind {
            "request" => {
                self.ensure_turn(entry, chat, out);
                let turn_id = self.turn_id();
                self.open_call_named(chat, &turn_id, call_id, tool_name, out);
                let title = self.title_of(call_id);
                let prompt = confirmation_prompt(&title, tool_name.unwrap_or(call_id));
                // The parameters ride the gate when their row has already
                // landed (the common order): the pending card is where a
                // client shows what it is being asked to approve.
                let tool_input = self.take_unpublished_input(call_id);
                out.push(Emitted::new(
                    chat,
                    StateAction::ChatToolCallReady(ChatToolCallReadyAction {
                        turn_id: turn_id.clone(),
                        tool_call_id: call_id.to_string(),
                        meta: Some(manox_meta(approval_meta(auth_id, &title))),
                        contributor: None,
                        intention: Some(title.clone()),
                        invocation_message: StringOrMarkdown::Plain(prompt.clone()),
                        tool_input: tool_input.map(ToolInput::Inline),
                        confirmation_title: Some(StringOrMarkdown::Plain(prompt.clone())),
                        risk_assessment: None,
                        edits: None,
                        editable: None,
                        confirmed: None,
                        options: Some(approval_options(tool_name)),
                    }),
                ));
                self.mark_awaiting(call_id, auth_id, &prompt);
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
            verdict,
            reason,
            input,
        } = ask;
        match kind {
            "request" => {
                self.ensure_turn(entry, chat, out);
                if let Some(open) = self.open.as_mut() {
                    open.open_asks.push(auth_id.to_string());
                }
                out.push(Emitted::new(
                    chat,
                    StateAction::ChatInputRequested(ChatInputRequestedAction {
                        request: ChatInputRequest {
                            id: auth_id.to_string(),
                            // The request-level display message carries the
                            // questions' `detail` support markdown — the wire
                            // has no per-question detail slot, so this is the
                            // only route support text reaches a client (a
                            // plan review is just an ask whose detail is the
                            // plan). No detail anywhere → no display message.
                            message: input.and_then(ask_details_markdown),
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
                if let Some(open) = self.open.as_mut() {
                    open.open_asks.retain(|id| id != auth_id);
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
        self.finish_turn(&open, chat, outcome, entry, false, out);
    }

    /// Close `open` and publish its terminal action.
    ///
    /// `steer_boundary` marks a completion that is not the run's settle edge:
    /// a mid-run steer resumes the run in a new turn, so a client that keys
    /// run-lifecycle work off the completion (flushing a follow-up queue, say)
    /// must read the `_meta.steerBoundary` flag before acting.
    fn finish_turn(
        &mut self,
        open: &OpenTurn,
        chat: &str,
        outcome: Outcome,
        entry: &Envelope,
        steer_boundary: bool,
        out: &mut Vec<Emitted>,
    ) {
        let duration = elapsed_ms(&open.started_at, &entry.timestamp);
        let failed = matches!(outcome, Outcome::Failed(_));
        let mut meta = json!({"entryId": entry.id, "outcome": outcome.tag()});
        if open.synthetic {
            meta["synthetic"] = json!(true);
        }
        if steer_boundary {
            meta["steerBoundary"] = json!(true);
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
        // Parked questions answer the same must-not-hang rule. The gate
        // journals a `cancelled` decision when a turn dies with a card
        // (`UserQuestionGate::discard`), so the normal replay settles the
        // part one row earlier; a crash before that row lands leaves the
        // decision missing, and an unanswered ask in an archived turn is
        // unanswerable by construction — the ahp reducer settles input
        // requests on the ACTIVE turn only. Emit the settlement here so the
        // replayed fold never carries a dead card.
        for request_id in &open.open_asks {
            out.push(Emitted::new(
                chat,
                StateAction::ChatInputCompleted(ChatInputCompletedAction {
                    request_id: request_id.clone(),
                    response: ChatInputResponseKind::Cancel,
                    answers: None,
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
        turn.calls.insert(
            call_id.to_string(),
            CallBook::new(tool_name.unwrap_or_default(), ""),
        );
    }

    fn publish_running(
        &mut self,
        chat: &str,
        turn_id: &str,
        call_id: &str,
        title: &str,
        out: &mut Vec<Emitted>,
    ) {
        // The call's journaled parameters ride the ready action — the one
        // place whose reducer lands `toolInput` on the running state. A
        // delta alone cannot: it parks on the streaming part and the ready
        // transition drops it, so a client folding the wire never saw the
        // command it was asked to display.
        let tool_input = self.take_unpublished_input(call_id);
        // An approval-first call already carries the verdict's reason; the
        // journal's "it ran" row must not flatten an explicit approval to
        // `NotNeeded`.
        let confirmed = self
            .call(call_id)
            .and_then(|call| call.confirmed.clone())
            .unwrap_or(ToolCallConfirmationReason::NotNeeded);
        out.push(Emitted::new(
            chat,
            StateAction::ChatToolCallReady(ChatToolCallReadyAction {
                turn_id: turn_id.to_string(),
                tool_call_id: call_id.to_string(),
                meta: None,
                contributor: None,
                intention: Some(title.to_string()),
                invocation_message: StringOrMarkdown::Plain(title.to_string()),
                tool_input: tool_input.map(ToolInput::Inline),
                confirmation_title: None,
                risk_assessment: None,
                edits: None,
                editable: None,
                // The journal says it ran, so confirmation was not needed — or
                // the gate's verdict already named the reason.
                confirmed: Some(confirmed.clone()),
                options: None,
            }),
        ));
        if let Some(call) = self.call_mut(call_id) {
            call.working = true;
            call.awaiting = false;
            call.confirmed = Some(confirmed);
        }
    }

    /// Deliver the call's parameters to a call the wire already moved past
    /// `streaming` — the approval-first order, where the gate opened before the
    /// `toolCall` row arrived, or a row that learned its parameters after its
    /// phase transition published. The reducer updates a pending confirmation
    /// in place (the ready stays `confirmed: None`) and refreshes a running
    /// call without moving it; a settled call is left alone.
    fn publish_input(
        &mut self,
        chat: &str,
        session: &str,
        turn_id: &str,
        call_id: &str,
        out: &mut Vec<Emitted>,
    ) {
        let Some((awaiting, working, settled)) = self
            .call(call_id)
            .map(|call| (call.awaiting, call.working, call.settled))
        else {
            return;
        };
        if settled || awaiting == working {
            return;
        }
        let Some(inline) = self.take_unpublished_input(call_id) else {
            return;
        };
        let Some((title, tool_name, output, auth_id, prompt, confirmed)) =
            self.call(call_id).map(|call| {
                (
                    call.title.clone(),
                    call.tool_name.clone(),
                    call.output.clone(),
                    call.auth_id.clone(),
                    call.pending_prompt.clone(),
                    call.confirmed.clone(),
                )
            })
        else {
            return;
        };
        // Every ready rebuilds the invocation message from the action, so the
        // catch-up restates it: the gate's own prompt while the card is parked
        // (a blank title here would replace what the user is reading), the
        // title once the call runs.
        let invocation = if awaiting {
            prompt.unwrap_or_else(|| {
                confirmation_prompt(
                    &title,
                    if tool_name.is_empty() {
                        call_id
                    } else {
                        tool_name.as_str()
                    },
                )
            })
        } else if !title.is_empty() {
            title.clone()
        } else if !tool_name.is_empty() {
            tool_name.clone()
        } else {
            call_id.to_string()
        };
        out.push(Emitted::new(
            chat,
            StateAction::ChatToolCallReady(ChatToolCallReadyAction {
                turn_id: turn_id.to_string(),
                tool_call_id: call_id.to_string(),
                meta: None,
                contributor: None,
                // The gate opened before the row, so the pending card's
                // intention was blank; the row's title fills it.
                intention: (awaiting && !title.is_empty()).then(|| title.clone()),
                invocation_message: StringOrMarkdown::Plain(invocation.clone()),
                tool_input: Some(ToolInput::Inline(inline)),
                confirmation_title: None,
                risk_assessment: None,
                edits: None,
                editable: None,
                confirmed: (!awaiting)
                    .then(|| confirmed.unwrap_or(ToolCallConfirmationReason::NotNeeded)),
                options: None,
            }),
        ));
        // A running part's content is rebuilt empty by the ready transition;
        // replay what the journal already delivered so the catch-up cannot
        // blank the output.
        if working && !output.is_empty() {
            out.push(Emitted::new(
                chat,
                StateAction::ChatToolCallContentChanged(ChatToolCallContentChangedAction {
                    turn_id: turn_id.to_string(),
                    tool_call_id: call_id.to_string(),
                    meta: None,
                    content: text_blocks(&output),
                }),
            ));
        }
        // The gate's session mirror is the surface a client may read instead of
        // the chat card; restate it so the approval-first catch-up lands on
        // both (the reducer replaces the request with the same id in place).
        if awaiting && let Some(auth_id) = auth_id {
            let mut request = self.pending_confirmation(
                call_id,
                (!tool_name.is_empty()).then_some(tool_name.as_str()),
                &auth_id,
            );
            request.invocation_message = StringOrMarkdown::Plain(invocation);
            out.push(Emitted::new(
                session,
                StateAction::SessionInputNeededSet(Box::new(SessionInputNeededSetAction {
                    request: SessionInputRequest::ToolConfirmation(
                        SessionToolConfirmationRequest {
                            id: auth_id,
                            chat: chat.to_string(),
                            turn_id: turn_id.to_string(),
                            tool_call: ToolCallConfirmationState::PendingConfirmation(request),
                        },
                    ),
                })),
            ));
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

    fn mark_awaiting(&mut self, call_id: &str, auth_id: &str, prompt: &str) {
        if let Some(call) = self.call_mut(call_id) {
            call.awaiting = true;
            call.working = false;
            call.auth_id = Some(auth_id.to_string());
            call.pending_prompt = Some(prompt.to_string());
        }
    }

    fn mark_resolved(&mut self, call_id: &str, approved: bool) {
        if let Some(call) = self.call_mut(call_id) {
            call.awaiting = false;
            call.auth_id = None;
            call.pending_prompt = None;
            if approved {
                call.working = true;
                call.confirmed = Some(ToolCallConfirmationReason::UserAction);
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
            tool_input: self
                .call(call_id)
                .and_then(|call| call.input.clone())
                .and_then(|input| inline_input(&input))
                .map(ToolInput::Inline),
            confirmation_title: None,
            risk_assessment: None,
            // The AHP 1.0 edit-preview face: the call's journaled parameters
            // become the edits a client renders before its verdict.
            edits: self
                .call(call_id)
                .and_then(|call| preview_edits(&call.tool_name, call.input.as_ref())),
            editable: None,
            options: Some(approval_options(Some(name))),
        }
    }

    fn call(&self, call_id: &str) -> Option<&CallBook> {
        self.open.as_ref()?.calls.get(call_id)
    }

    fn call_mut(&mut self, call_id: &str) -> Option<&mut CallBook> {
        self.open.as_mut()?.calls.get_mut(call_id)
    }

    /// The call's parameters for their one wire publication, marking them
    /// published: a `ChatToolCallReady` is the only emission whose reducer
    /// lands `toolInput` past the streaming state, so each parameter set
    /// publishes at most once. A call with nothing to publish (no row yet,
    /// or parameters absent) stays unpublished — the row that carries them
    /// publishes through here.
    fn take_unpublished_input(&mut self, call_id: &str) -> Option<String> {
        let call = self.call_mut(call_id)?;
        if call.input_published {
            return None;
        }
        let inline = call.input.as_ref().and_then(inline_input)?;
        call.input_published = true;
        Some(inline)
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

/// The AHP 1.0 edit-preview face of one tool call's journaled parameters.
///
/// `Write` is a creation (an after side only, counted from the content's
/// lines); `Edit` is a hashline patch parsed for its per-file footprints —
/// the sides name the file (a client fetches content through the
/// `resourceRead` plane), and the counts come from the patch's own
/// statically-known spans: `SWAP`/`DEL`/`INS`/`CUT` count exactly, the
/// block ops count their inline bodies, and the directions whose resolution
/// needs file content or engine clipboard state the projection never sees
/// (block-deleted/replaced spans, pasted bodies) carry `None` — an unknown
/// count, not a zero — so a client renders `+?`/`-?` instead of reading "no
/// changes". Any other tool, a missing parameter, or an unparseable patch
/// answers `None` — no preview is better than a wrong one.
fn preview_edits(name: &str, input: Option<&Value>) -> Option<FileEditCollection> {
    use manox_harness::core::hashline::{FileOp, Op};
    let input = input?;
    let side = |uri: &str| {
        Some(FileEditSide {
            uri: file_uri(uri),
            content: ContentRef {
                uri: file_uri(uri),
                size_hint: None,
                content_type: None,
                nonce: None,
            },
        })
    };
    match name {
        "Write" => {
            let path = input.get("path")?.as_str()?;
            let content = input.get("content").and_then(Value::as_str)?;
            let added = content.lines().count() as i64;
            let after = side(path);
            Some(FileEditCollection {
                items: vec![FileEdit {
                    before: None,
                    after,
                    diff: Some(FileEditDiffStats {
                        added: Some(added),
                        // A Write may overwrite an existing file; whether it
                        // removes lines, and how many, is file state the
                        // projection never sees.
                        removed: None,
                    }),
                }],
            })
        }
        "Edit" => {
            let patch = input.get("patch")?.as_str()?;
            let parsed = manox_harness::core::hashline::parse_patch(patch).ok()?;
            // Sums into an optional count; `None` (an unknowable direction) is
            // sticky — unknown plus known stays unknown.
            fn bump(acc: &mut Option<i64>, n: i64) {
                *acc = acc.map(|c| c + n);
            }
            let items = parsed
                .files
                .iter()
                .map(|file| {
                    let path = file.path.display().to_string();
                    let mut added: Option<i64> = Some(0);
                    let mut removed: Option<i64> = Some(0);
                    for op in &file.ops {
                        match op {
                            Op::Swap { start, end, body } => {
                                bump(&mut removed, (*end - *start + 1) as i64);
                                bump(&mut added, body.len() as i64);
                            }
                            Op::Del { start, end } | Op::Cut { start, end } => {
                                bump(&mut removed, (*end - *start + 1) as i64);
                            }
                            // The block ops' inline bodies are known, so the
                            // insertion side still counts; the content-resolved
                            // span they replace or follow is not, so a
                            // `SwapBlk` marks the REMOVED direction unknown.
                            Op::Ins { body, .. } | Op::InsBlkPost { body, .. } => {
                                bump(&mut added, body.len() as i64);
                            }
                            Op::SwapBlk { body, .. } => {
                                bump(&mut added, body.len() as i64);
                                removed = None;
                            }
                            // The deleted span of a block deletion and the
                            // pasted body of a clipboard paste are the fully
                            // unknowable directions.
                            Op::DelBlk { .. } | Op::CutBlk { .. } => {
                                removed = None;
                            }
                            Op::Paste { .. } => {
                                added = None;
                            }
                        }
                    }
                    // `REM` is a deletion (before only); a `MV` keeps both
                    // sides on the source path — the destination is the
                    // engine's business until it lands.
                    let (before, after) = match file.file_op {
                        // A REM deletes the whole file; its line count is file
                        // state the projection never sees, so the removed
                        // direction stays unknown.
                        Some(FileOp::Rem) => {
                            removed = None;
                            (side(&path), None)
                        }
                        _ => (side(&path), side(&path)),
                    };
                    FileEdit {
                        before,
                        after,
                        diff: Some(FileEditDiffStats { added, removed }),
                    }
                })
                .collect();
            Some(FileEditCollection { items })
        }
        _ => None,
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
/// The network approval carries the two amendment shapes (session /
/// persistent host grants) on top of the bare pair — the ids match the
/// verdict strings the gateway maps back, and clients render the set
/// verbatim.
fn approval_options(tool_name: Option<&str>) -> Vec<ConfirmationOption> {
    let mut options = vec![ConfirmationOption {
        id: "allow_once".to_string(),
        label: "Allow once".to_string(),
        kind: ConfirmationOptionKind::Approve,
        group: Some(0),
    }];
    if tool_name == Some(manox_harness::sandbox::NETWORK_APPROVAL_TOOL) {
        options.push(ConfirmationOption {
            id: "allow_host_session".to_string(),
            label: "Allow this host for the session".to_string(),
            kind: ConfirmationOptionKind::Approve,
            group: Some(0),
        });
        options.push(ConfirmationOption {
            id: "allow_host_persistent".to_string(),
            label: "Always allow this host".to_string(),
            kind: ConfirmationOptionKind::Approve,
            group: Some(0),
        });
    }
    options.push(ConfirmationOption {
        id: "deny".to_string(),
        label: "Deny".to_string(),
        kind: ConfirmationOptionKind::Deny,
        group: Some(1),
    });
    options
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

/// The transcript face of a UI note card: an annotation stamped
/// `_meta[ext::META_KEY]["uiNoteKind"]` is the desktop app's note card — its
/// text is the entries joined, and the kind plus optional tool anchoring ride
/// the same `x-manox` slot on the emitted part. Plain review annotations
/// return `None`; they live on the annotations channel alone.
fn annotation_note_face(annotation: &ahp_types::state::Annotation) -> Option<(String, Value)> {
    let xmanox = annotation
        .meta
        .as_ref()?
        .get(ext::META_KEY)?
        .get("uiNoteKind")
        .and_then(Value::as_str)?;
    let text = annotation
        .entries
        .iter()
        .map(|entry| entry.text.as_text())
        .collect::<Vec<_>>()
        .join("\n");
    let mut payload = json!({ "annotationId": annotation.id, "uiNoteKind": xmanox });
    if let Some(tool_call_id) = annotation
        .meta
        .as_ref()
        .and_then(|meta| meta.get(ext::META_KEY))
        .and_then(|meta| meta.get("toolCallId"))
        .and_then(Value::as_str)
    {
        payload["toolCallId"] = json!(tool_call_id);
    }
    Some((text, payload))
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
    fn a_background_task_row_rides_the_snapshot_key_into_the_registry_state() {
        // One-field payload on purpose: this test pins the wire KEY (the row
        // rides nested under `snapshot`) and its route into the fold — not
        // the row's shape, which no literal here could faithfully claim. The
        // shape's authority is the host golden
        // (`task_snapshot_serialization_is_wire_stable`), and the chain from
        // that real shape runs in manox-ahp-runtime's
        // `background_task_snapshot_flows_from_host_shape_to_extension_state`.
        let snapshot = json!({"task_id": "bg_7"});
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
                [("bg_7".to_string(), snapshot.clone())]
                    .into_iter()
                    .collect()
            )
        );
        // …and the field survives a state round-trip for reconnecting clients.
        let encoded = serde_json::to_value(&state).expect("state serializes");
        assert_eq!(encoded["backgroundTasks"]["bg_7"], snapshot);
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
            task_id: "bg_7".into(),
            kind: TaskKind::BackgroundBash,
            owner_thread_id: "t-1".into(),
            description: "cargo build".into(),
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
            state.background_tasks.as_ref().and_then(|m| m.get("bg_7")),
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

    /// The child-session rows the spawn writes must fold into reasoning and
    /// tool cards, interleaved in emission order: a thinking delta before
    /// its message row, and a tool call before its result.
    #[test]
    fn child_event_rows_fold_to_reasoning_and_settled_tool_cards() {
        use manox_harness::ext::subagent::journal::child_journal_rows;
        use manox_harness::tool::AgentToolResult;
        use manox_harness::types::{AgentEvent, AgentMessage, AssistantMessageEvent, StopReason};

        let assistant_message = AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "reading it".into(),
                signature: None,
            }],
            model: "test-model".into(),
            provider: "test-provider".into(),
            api: "anthropic".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::ToolUse),
            raw_stop_reason: None,
            usage: Box::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        let events = vec![
            AgentEvent::AgentStart,
            // The persistence middleware writes the user row; the event
            // mapping does not.
            AgentEvent::MessageEnd {
                message: Box::new(AgentMessage::user("find the bug")),
            },
            AgentEvent::MessageUpdate {
                message: Box::new(assistant_message.clone()),
                assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                    content_index: 0,
                    delta: "weighing options".into(),
                },
            },
            AgentEvent::MessageUpdate {
                message: Box::new(assistant_message.clone()),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 1,
                    delta: "reading the file".into(),
                },
            },
            AgentEvent::MessageEnd {
                message: Box::new(assistant_message),
            },
            AgentEvent::ToolExecutionStart {
                tool_call_id: "call-1".into(),
                tool_name: "Read".into(),
                arguments: json!({"path": "src/lib.rs"}),
            },
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "call-1".into(),
                tool_name: "Read".into(),
                result: AgentToolResult::text("contents"),
                is_error: false,
            },
            AgentEvent::AgentEnd {
                messages: Vec::new(),
            },
        ];

        let mut entries = Vec::new();
        let mut seq = 0u64;
        for event in &events {
            if let AgentEvent::MessageEnd { message } = event {
                entries.push(SessionTreeEntry::Message {
                    id: format!("m-{seq}"),
                    parent_id: None,
                    timestamp: chrono::Utc::now(),
                    message: (**message).clone(),
                    origin: None,
                });
                seq += 1;
                continue;
            }
            for (kind, payload) in child_journal_rows(event) {
                entries.push(
                    SessionTreeEntry::from_kind_payload(
                        kind,
                        format!("e-{seq}"),
                        None,
                        chrono::Utc::now(),
                        payload,
                    )
                    .expect("the mapped row matches the typed vocabulary"),
                );
                seq += 1;
            }
        }

        let mut translator = Translator::new();
        let mut actions = Vec::new();
        for (seq, entry) in entries.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                if emitted.channel == "ahp-chat:/c-1" {
                    actions.push(emitted.action);
                }
            }
        }

        let reasoning = actions
            .iter()
            .position(|action| {
                matches!(action, StateAction::ChatReasoning(part) if part.content.contains("weighing options"))
            })
            .expect("the thinking delta folds to a reasoning part");
        let call_start = actions
            .iter()
            .position(|action| {
                matches!(action, StateAction::ChatToolCallStart(call) if call.tool_call_id == "call-1")
            })
            .expect("the tool call folds to a card");
        let call_complete = actions
            .iter()
            .position(|action| {
                matches!(action, StateAction::ChatToolCallComplete(call)
                    if call.tool_call_id == "call-1" && call.result.success)
            })
            .expect("the tool result settles the card");
        assert!(
            reasoning < call_start && call_start < call_complete,
            "rows interleave in emission order: {actions:?}"
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
/// folded into the turn it interrupted puts the reply before the prompt, a
/// stop row closed mid-loop splits one turn per tool round, a textless ask
/// strands the model waiting forever.
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
            // The row id IS the client's steer id (the kernel appends a
            // caller-minted user message under it); the row carries no
            // `origin` — that pin belongs to the submit path, and
            // `AgentMessage::User.id` never crosses the wire, so the row id
            // is the only durable identity the fold can read.
            kernel(json!({
                "type": "message",
                "id": "steer-1",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "use the fast suite"}]},
            })),
            kernel(json!({"type": "agent_text_delta", "id": "sentry-4",
                          "delta": "Using the fast suite."})),
            kernel(json!({"type": "turn_finish", "id": "sentry-5",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
        ]
    }

    /// The markdown a turn rendered, for the partition assertions.
    fn markdown_text(parts: &[ahp_types::state::ResponsePart]) -> String {
        parts
            .iter()
            .filter_map(|part| match part {
                ahp_types::state::ResponsePart::Markdown(markdown) => {
                    Some(markdown.content.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// A mid-run steer divides the transcript where it was injected.
    ///
    /// The pre-steer round settles as its own turn, with the content it had
    /// produced, and the steer opens the next turn. Journal order is reading
    /// order on every fold — live and replayed — so a client never has to
    /// reconstruct the steer from a retire-time echo and guess where it
    /// belongs.
    #[test]
    fn a_mid_run_steer_partitions_the_transcript() {
        let state = mid_run_steer_state();
        assert_eq!(
            state.turns.len(),
            2,
            "the run folds as two turns: {:?}",
            state.turns
        );
        let before = &state.turns[0];
        assert_eq!(
            before.message.text, "run the tests",
            "the pre-steer turn keeps the submission it opened with"
        );
        assert_eq!(
            markdown_text(&before.response_parts),
            "Starting.",
            "the pre-steer turn keeps exactly the content produced before the steer"
        );
        assert_eq!(before.state, ahp_types::state::TurnState::Complete);

        let after = &state.turns[1];
        assert_eq!(
            after.message.text, "use the fast suite",
            "the steer opens its own turn with the steer text"
        );
        assert_eq!(
            markdown_text(&after.response_parts),
            "Using the fast suite.",
            "the reply to the steer lands in the steer's turn, after it"
        );
        assert_eq!(after.state, ahp_types::state::TurnState::Complete);
        assert!(
            state.active_turn.is_none(),
            "the turn finish closed the steer's turn too"
        );
    }

    /// The steer's `turnStarted` carries the client's steer id as
    /// `queuedMessageId`, and the steer row publishes no pending message.
    ///
    /// The id is the one the client's optimistic echo and the host's `steer`
    /// intent already use; it is how the client retires the card and how the
    /// reducer clears a wire-opened steering slot. A projector-minted id would
    /// match neither.
    #[test]
    fn the_steer_turn_started_carries_the_clients_steer_id() {
        let mut translator = Translator::new();
        let mut started = Vec::new();
        let mut pending = Vec::new();
        let mut completed = Vec::new();
        for (seq, entry) in mid_run_steer_records().into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                match &emitted.action {
                    StateAction::ChatTurnStarted(action) => {
                        started.push((
                            action.turn_id.clone(),
                            action.message.text.clone(),
                            action.queued_message_id.clone(),
                        ));
                    }
                    StateAction::ChatPendingMessageSet(set) => {
                        pending.push((format!("{:?}", set.kind), set.id.clone()));
                    }
                    StateAction::ChatTurnComplete(action) => {
                        let meta = action.meta.as_ref().and_then(|meta| meta.get("x-manox"));
                        completed.push((
                            action.turn_id.clone(),
                            meta.and_then(|meta| meta.get("steerBoundary"))
                                .and_then(Value::as_bool),
                        ));
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(
            started,
            [
                (
                    "t-sentry-1".to_string(),
                    "run the tests".to_string(),
                    Some("m-sentry-0".to_string()),
                ),
                (
                    "t-steer-1".to_string(),
                    "use the fast suite".to_string(),
                    Some("steer-1".to_string()),
                ),
            ],
            "one turn per injected round, the steer turn stamped with the client's id"
        );
        // The opening submission is a genuine queue entry drained by the turn
        // it starts; the steer is a transcript row, not a pending message.
        assert_eq!(
            pending,
            [("Queued".to_string(), "m-sentry-0".to_string())],
            "the steer row must not publish a pending message: {pending:?}"
        );
        // The partition's completion is not the run's settle edge: it carries
        // the boundary flag, while the run's own finish does not.
        assert_eq!(
            completed,
            [
                ("t-sentry-1".to_string(), Some(true)),
                ("t-steer-1".to_string(), None),
            ],
            "only the steer boundary is flagged as mid-run: {completed:?}"
        );
    }

    /// Consecutive steers each open a turn: every steer row is a user row a
    /// reader must see, and folding two into one turn would drop the first.
    #[test]
    fn consecutive_steers_each_open_a_turn() {
        let records = [
            kernel(json!({
                "type": "message",
                "id": "u-0",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "run the tests"}]},
                "origin": "rpc-submit",
            })),
            kernel(json!({"type": "turn_start", "id": "ts-1"})),
            kernel(json!({"type": "agent_text_delta", "id": "d-2", "delta": "Starting."})),
            kernel(json!({
                "type": "message",
                "id": "steer-a",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "use the fast suite"}]},
                "origin": "steer-a",
            })),
            kernel(json!({
                "type": "message",
                "id": "steer-b",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "and skip the flaky one"}]},
                "origin": "steer-b",
            })),
            kernel(json!({"type": "agent_text_delta", "id": "d-5", "delta": "Done."})),
            kernel(json!({"type": "turn_finish", "id": "tf-6",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
        ];
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        let messages: Vec<&str> = state
            .turns
            .iter()
            .map(|turn| turn.message.text.as_str())
            .collect();
        assert_eq!(
            messages,
            [
                "run the tests",
                "use the fast suite",
                "and skip the flaky one"
            ],
            "each user row opens a turn, in journal order"
        );
        assert_eq!(
            markdown_text(&state.turns[1].response_parts),
            "",
            "the first steer's turn carries no reply: the next steer cut it off"
        );
        assert_eq!(
            markdown_text(&state.turns[2].response_parts),
            "Done.",
            "the reply follows the last steer"
        );
    }

    /// The steer turn's `queuedMessageId` is the ROW id, not a
    /// projector-minted pending id.
    ///
    /// A steer row carries no `origin` (the origin pin is the submit path's)
    /// and its `AgentMessage::User.id` is not serialized, so the row id is the
    /// only durable identity: for a client steer that id IS the id its
    /// `pendingMessageSet{kind: steering}` used. Prefixing it with `m-` names
    /// nothing the client (or the reducer's exact-match slot clear) holds.
    #[test]
    fn the_steer_turn_names_the_row_id_as_the_queued_id() {
        let mut translator = Translator::new();
        let mut started = Vec::new();
        for (seq, entry) in mid_run_steer_records().into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                if let StateAction::ChatTurnStarted(action) = &emitted.action
                    && action.message.text == "use the fast suite"
                {
                    started.push(action.queued_message_id.clone());
                }
            }
        }
        assert_eq!(
            started,
            [Some("steer-1".to_string())],
            "the steer turn names the journal row id (the client's steer id)"
        );
    }

    /// A user row injected with no client identity (a peer delivery, an
    /// agent-authored row) partitions too: the journal cannot tell it from a
    /// client steer, and journal order is reading order either way. Its
    /// `queuedMessageId` is its storage id — no client slot or queued entry
    /// matches, so the reducer's clear is a no-op.
    #[test]
    fn an_injected_row_partitions_with_its_storage_id_as_the_queued_id() {
        let records = [
            kernel(json!({"type": "turn_start", "id": "ts-1"})),
            kernel(json!({
                "type": "message",
                "id": "u-1",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "run the tests"}]},
            })),
            kernel(json!({"type": "agent_text_delta", "id": "d-2", "delta": "Starting."})),
            kernel(json!({
                "type": "message",
                "id": "peer-7",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "the lead says hi"}]},
            })),
            kernel(json!({"type": "agent_text_delta", "id": "d-4", "delta": "Noted."})),
            kernel(json!({"type": "turn_finish", "id": "tf-5",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
        ];
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        let mut queued_ids = Vec::new();
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                if let StateAction::ChatTurnStarted(action) = &emitted.action {
                    queued_ids.push((
                        action.message.text.clone(),
                        action.queued_message_id.clone(),
                    ));
                }
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(
            queued_ids.last(),
            Some(&("the lead says hi".to_string(), Some("peer-7".to_string()))),
            "an injected row names its storage id, matching no client slot: {queued_ids:?}"
        );
        assert_eq!(state.turns.len(), 2, "the injected row opens its own turn");
        assert_eq!(
            markdown_text(&state.turns[1].response_parts),
            "Noted.",
            "the reply follows the injected row"
        );
    }

    /// A parked steer consumed at the next run's start partitions BEFORE any
    /// assistant content, so the pre-steer turn completes with only its
    /// opening message (a boundary, not a run end). This is the wake flow:
    /// the journal cannot fold the steer into the prompt's turn (AHP gives a
    /// turn one opening message), and keeping a wire-only slot would strand
    /// it on replay. Pinned so the boundary semantics are explicit.
    #[test]
    fn a_parked_steer_consumed_at_run_start_partitions_a_contentless_turn() {
        let records = [
            // The submit's durable row lands BEFORE turn_start (K5), so the
            // turn opens carrying it; the steer then arrives with no content.
            kernel(json!({
                "type": "message",
                "id": "u-0",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "wake the run"}]},
            })),
            kernel(json!({"type": "turn_start", "id": "ts-1"})),
            kernel(json!({
                "type": "message",
                "id": "steer-1",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "and skip the flaky one"}]},
            })),
            kernel(json!({"type": "agent_text_delta", "id": "d-3", "delta": "Skipped."})),
            kernel(json!({"type": "turn_finish", "id": "tf-4",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
        ];
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        let mut boundaries: Vec<(String, bool)> = Vec::new();
        let mut steer_ids = Vec::new();
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                match &emitted.action {
                    StateAction::ChatTurnStarted(action)
                        if action.message.text == "and skip the flaky one" =>
                    {
                        steer_ids.push(action.queued_message_id.clone());
                    }
                    StateAction::ChatTurnComplete(action) => {
                        let boundary = action
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.get("x-manox"))
                            .and_then(|meta| meta.get("steerBoundary"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        boundaries.push((action.turn_id.clone(), boundary));
                    }
                    _ => {}
                }
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(
            steer_ids,
            [Some("steer-1".to_string())],
            "the wake steer names the client's id: {:?}",
            steer_ids
        );
        assert_eq!(
            boundaries,
            [
                ("t-ts-1".to_string(), true),
                ("t-steer-1".to_string(), false),
            ],
            "the wake boundary is flagged; the run's own finish is not"
        );
        let pre = &state.turns[0];
        assert_eq!(pre.message.text, "wake the run");
        assert_eq!(
            markdown_text(&pre.response_parts),
            "",
            "the pre-steer turn produced nothing before the steer"
        );
        assert_eq!(
            pre.state,
            ahp_types::state::TurnState::Complete,
            "it settles as a boundary completion"
        );
        assert_eq!(
            markdown_text(&state.turns[1].response_parts),
            "Skipped.",
            "the reply lands under the steer the run actually answered"
        );
    }

    /// The wire's steering slot retires on the partitioning `turnStarted`.
    ///
    /// A live steer reaches a client as `pendingMessageSet{Steering}` first
    /// (its optimistic echo); the journal row then folds into a `turnStarted`
    /// carrying the same id, which the reducer matches against the slot. The
    /// slot's wire lifetime is therefore unchanged by the partition — it still
    /// opens on the Set and closes on the row.
    #[test]
    fn the_wire_steering_slot_retires_on_the_partitioning_turn_started() {
        let mut state = manox_ahp::channels::chat::initial("c-1");
        ahp::reducers::apply_action_to_chat(
            &mut state,
            &StateAction::ChatPendingMessageSet(ChatPendingMessageSetAction {
                kind: PendingMessageKind::Steering,
                id: "steer-1".to_string(),
                message: Message {
                    text: "use the fast suite".to_string(),
                    origin: MessageOrigin {
                        kind: MessageKind::User,
                    },
                    attachments: None,
                    model: None,
                    agent: None,
                    meta: None,
                },
            }),
        );
        assert!(state.steering_message.is_some(), "the echo opened the slot");

        let mut translator = Translator::new();
        for (seq, entry) in mid_run_steer_records().into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert!(
            state.steering_message.is_none(),
            "the steer's turnStarted retires the slot by the same id: {:?}",
            state.steering_message
        );
    }

    /// Restore replays the same journal into the same fold.
    #[test]
    fn the_steer_partition_is_deterministic_across_replays() {
        assert_eq!(
            mid_run_steer_state(),
            mid_run_steer_state(),
            "two independent folds of one journal must agree"
        );
    }

    /// A submission after a closed turn is still a queued message, not a
    /// steer: with no turn open there is nothing to inject into, and the next
    /// `turn_start` carries it as the opening message.
    #[test]
    fn a_submission_after_a_closed_turn_stays_queued() {
        let records = [
            kernel(json!({"type": "turn_start", "id": "ts-1"})),
            kernel(json!({
                "type": "message",
                "id": "u-1",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "first"}]},
                "origin": "rpc-1",
            })),
            kernel(json!({"type": "agent_text_delta", "id": "d-2", "delta": "done"})),
            kernel(json!({"type": "turn_finish", "id": "tf-3",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
            kernel(json!({
                "type": "message",
                "id": "u-4",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "second"}]},
                "origin": "rpc-2",
            })),
        ];
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        let mut sets = Vec::new();
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                if let StateAction::ChatPendingMessageSet(set) = &emitted.action {
                    sets.push((format!("{:?}", set.kind), set.id.clone()));
                }
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(
            sets,
            [("Queued".to_string(), "m-u-4".to_string())],
            "an idle submission queues; it must not be mistaken for a steer"
        );
        assert_eq!(
            state.turns.len(),
            1,
            "only the closed turn is archived: {:?}",
            state.turns
        );
        assert!(
            state.active_turn.is_none(),
            "the second submission waits for its turn"
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

    /// A consumed steer leaves no pending residue of either kind.
    ///
    /// The steer's slot is a wire-side fact: the client's Set opens it and the
    /// injection receipt closes it. If the fold also published it, restore
    /// replay would reopen a slot no removal ever closes. The partition
    /// publishes the row as a turn instead, so a replayed journal carries the
    /// text exactly once — in the transcript.
    #[test]
    fn a_consumed_steer_leaves_no_pending_residue() {
        let state = mid_run_steer_state();
        let queued = state.queued_messages.unwrap_or_default();
        assert!(
            queued.is_empty(),
            "the steer must not remain queued: a queued entry is re-injected as a \
             fresh user message by the next `turnStarted`, running the same text \
             twice (residue: {queued:?})"
        );
        assert!(
            state.steering_message.is_none(),
            "a replayed journal must not strand a steering slot the injection \
             receipt already closed live: {:?}",
            state.steering_message
        );
    }

    /// A later turn does not inherit the steer.
    #[test]
    fn a_steer_is_not_replayed_by_a_later_turn() {
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        for (seq, entry) in mid_run_steer_records().into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }

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
        let active = state.active_turn.as_ref().expect("the turn opened");
        assert_eq!(
            active.message.text, "",
            "the steer was consumed by the turn it opened, not carried into the \
             next one"
        );
        assert!(
            state.steering_message.is_none(),
            "no slot is left for a removal to match: {:?}",
            state.steering_message
        );
    }

    /// An idle steer's wake retires the steering slot by id, not by inference.
    ///
    /// The `steer` intent on an idle thread opens its own turn: the receipt
    /// says `injected: false`, so the host publishes no removal — the woken
    /// turn IS the consumption. What retires the client's card is the wake
    /// itself, and it can only do that by id: the run's opening user row
    /// carries the steer id as its `originRpc` (the server pins it), the
    /// initiating re-issue names it as `queuedMessageId`, and the reducer
    /// matches that against the steering slot (#945).
    #[test]
    fn an_idle_wake_stamps_the_opening_row_with_the_client_id() {
        let steer = |state: &mut ahp_types::state::ChatState| {
            ahp::reducers::apply_action_to_chat(
                state,
                &StateAction::ChatPendingMessageSet(ChatPendingMessageSetAction {
                    kind: PendingMessageKind::Steering,
                    id: "steer-1".to_string(),
                    message: Message {
                        text: "wake up".to_string(),
                        origin: MessageOrigin {
                            kind: MessageKind::User,
                        },
                        attachments: None,
                        model: None,
                        agent: None,
                        meta: None,
                    },
                }),
            );
        };
        let wake = |origin: Option<&str>| {
            let mut row = json!({
                "type": "message",
                "id": "sentry-1",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "wake up"}]},
            });
            if let Some(origin) = origin {
                row["origin"] = json!(origin);
            }
            [
                kernel(json!({"type": "turn_start", "id": "sentry-0"})),
                kernel(row),
            ]
        };

        // The stamped wake: the slot retires and the turn opens with the text.
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        steer(&mut state);
        for (seq, entry) in wake(Some("steer-1")).into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        let turn = state.active_turn.as_ref().expect("the wake opened a turn");
        assert_eq!(
            turn.message.text, "wake up",
            "the woken turn opens with the steer's text"
        );
        assert!(
            state.steering_message.is_none(),
            "the wake consumed the steer by id: {:?}",
            state.steering_message
        );

        // The anonymous row this pin exists to remove: no client id, so the
        // re-issue mints `m-{entry}`, matches nothing, and the slot survives
        // the wake — the residue a later turn would inherit and clients could
        // only retire by matching row text.
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        steer(&mut state);
        for (seq, entry) in wake(None).into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(
            state.steering_message.as_ref().map(|m| m.id.as_str()),
            Some("steer-1"),
            "without the id the wake retires nothing"
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

    /// A streamed reply whose tail deltas lost the journal-write race against
    /// the settled row.
    ///
    /// The deltas and the settled row reach the journal through different
    /// write paths (deltas via the notice tap's actor queue, the row via the
    /// persistence middleware), so the row can overtake the reply's last
    /// deltas. Those deltas are the reply's own suffix — the settled text
    /// already carries them — so they must rejoin the part they streamed
    /// into: a second markdown part here is one reply the client renders as
    /// two, and part ids are replay-stable, so replaying the already-written
    /// journal keeps it split. The fixture is the real journal's shape: two
    /// deltas, the settled row (full text, usage), the tail deltas, then
    /// `stop` / `turn_finish`.
    #[test]
    fn tail_text_deltas_after_a_streamed_settled_row_rejoin_the_same_part() {
        let records = [
            kernel(json!({
                "type": "message",
                "id": "sentry-0",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "check the ingress"}]},
                "origin": "rpc-1",
            })),
            kernel(json!({"type": "agent_text_delta", "id": "sentry-1",
                          "delta": "…则 ALB 控制台手动加 "})),
            kernel(json!({"type": "agent_text_delta", "id": "sentry-2", "delta": "44"})),
            kernel(json!({
                "type": "message",
                "id": "sentry-3",
                "message": {"role": "assistant",
                            "content": [{"type": "text",
                                         "text": "…则 ALB 控制台手动加 443 转发规则。"}],
                            "model": "test-model", "provider": "test", "api": "anthropic",
                            "usage": {"input": 120, "output": 30}},
            })),
            kernel(json!({"type": "agent_text_delta", "id": "sentry-4", "delta": "3"})),
            kernel(json!({"type": "agent_text_delta", "id": "sentry-5",
                          "delta": " 转发规则。"})),
            kernel(json!({"type": "stop", "id": "sentry-6", "reason": "end_turn"})),
            kernel(json!({"type": "turn_finish", "id": "sentry-7",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
        ];

        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        let mut markdown_parts = Vec::new();
        let mut delta_part_ids = Vec::new();
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                match &emitted.action {
                    StateAction::ChatResponsePart(part) => {
                        if let ResponsePart::Markdown(m) = &part.part {
                            markdown_parts.push((m.id.clone(), m.content.clone()));
                        }
                    }
                    StateAction::ChatDelta(delta) => delta_part_ids.push(delta.part_id.clone()),
                    _ => {}
                }
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(
            markdown_parts.len(),
            1,
            "one streamed reply is one markdown part, tail included: {markdown_parts:?}"
        );
        assert!(
            !delta_part_ids.is_empty(),
            "the fixture streamed deltas for the tail to trail"
        );
        let (part_id, _) = &markdown_parts[0];
        assert!(
            delta_part_ids.iter().all(|id| id == part_id),
            "every delta appends into the run's own part: deltas {delta_part_ids:?} vs part {part_id}"
        );
        let turn = state
            .turns
            .last()
            .expect("the turn_finish archived the turn");
        let markdown: Vec<_> = turn
            .response_parts
            .iter()
            .filter_map(|p| match p {
                ResponsePart::Markdown(m) => Some(m.content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            markdown,
            ["…则 ALB 控制台手动加 443 转发规则。"],
            "the rejoined part is exactly the settled reply text: {markdown:?}"
        );
    }

    /// The thinking mirror: a reply that streamed only reasoning keeps its
    /// reasoning run across the streamed row, so a thinking tail that lost
    /// the write race rejoins the part instead of minting a second one. The
    /// row is the first writer of the reply's TEXT (none streamed), so its
    /// text still publishes as the reply's markdown part.
    #[test]
    fn a_thinking_tail_after_a_streamed_settled_row_stays_in_its_part() {
        let records = [
            kernel(json!({
                "type": "message",
                "id": "sentry-0",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "plan the ports"}]},
                "origin": "rpc-1",
            })),
            kernel(json!({"type": "agent_thinking_delta", "id": "sentry-1",
                          "delta": "weighing 443 vs 8443."})),
            kernel(json!({
                "type": "message",
                "id": "sentry-2",
                "message": {"role": "assistant",
                            "content": [{"type": "text", "text": "Use 443."}],
                            "model": "test-model", "provider": "test", "api": "anthropic",
                            "usage": {"input": 120, "output": 30}},
            })),
            kernel(json!({"type": "agent_thinking_delta", "id": "sentry-3",
                          "delta": " 8443 needs the upstream too."})),
            kernel(json!({"type": "stop", "id": "sentry-4", "reason": "end_turn"})),
            kernel(json!({"type": "turn_finish", "id": "sentry-5",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
        ];

        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        let mut reasoning_parts = Vec::new();
        let mut reasoning_ids = Vec::new();
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                match &emitted.action {
                    StateAction::ChatResponsePart(part) => {
                        if let ResponsePart::Reasoning(r) = &part.part {
                            reasoning_parts.push((r.id.clone(), r.content.clone()));
                        }
                    }
                    StateAction::ChatReasoning(reasoning) => {
                        reasoning_ids.push(reasoning.part_id.clone())
                    }
                    _ => {}
                }
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(
            reasoning_parts.len(),
            1,
            "one streamed reasoning run is one part, tail included: {reasoning_parts:?}"
        );
        let (part_id, _) = &reasoning_parts[0];
        assert!(
            reasoning_ids.iter().all(|id| id == part_id),
            "every reasoning delta appends into the run's own part: {reasoning_ids:?} vs {part_id}"
        );
        let turn = state
            .turns
            .last()
            .expect("the turn_finish archived the turn");
        let reasoning: Vec<_> = turn
            .response_parts
            .iter()
            .filter_map(|p| match p {
                ResponsePart::Reasoning(r) => Some(r.content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning,
            ["weighing 443 vs 8443. 8443 needs the upstream too."],
            "the rejoined part carries the full thinking run: {reasoning:?}"
        );
        let markdown = turn
            .response_parts
            .iter()
            .filter(|p| matches!(p, ResponsePart::Markdown(_)))
            .count();
        assert_eq!(
            markdown, 1,
            "the row is the text's first writer and still publishes the reply's markdown"
        );
    }

    /// A settled row that is the reply's first writer closes the runs: a
    /// delta arriving after it opens a fresh part, not the row's own. This
    /// is the boundary the streamed-row exception must not erode — a fresh
    /// reply's text must not silently merge into the previous row's part.
    #[test]
    fn a_delta_after_a_settled_first_writer_row_opens_a_fresh_part() {
        let records = [
            kernel(json!({
                "type": "message",
                "id": "sentry-0",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "answer plainly"}]},
                "origin": "rpc-1",
            })),
            kernel(json!({
                "type": "message",
                "id": "sentry-1",
                "message": {"role": "assistant",
                            "content": [{"type": "text", "text": "The port is 443."}],
                            "model": "test-model", "provider": "test", "api": "anthropic",
                            "usage": {"input": 120, "output": 30}},
            })),
            kernel(json!({"type": "agent_text_delta", "id": "sentry-2",
                          "delta": "Also 8443."})),
            kernel(json!({"type": "stop", "id": "sentry-3", "reason": "end_turn"})),
            kernel(json!({"type": "turn_finish", "id": "sentry-4",
                          "cancelled": false, "failed": false, "strandedSteerIds": []})),
        ];

        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        let mut markdown_parts = Vec::new();
        let mut delta_part_ids = Vec::new();
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                match &emitted.action {
                    StateAction::ChatResponsePart(part) => {
                        if let ResponsePart::Markdown(m) = &part.part {
                            markdown_parts.push(m.id.clone());
                        }
                    }
                    StateAction::ChatDelta(delta) => delta_part_ids.push(delta.part_id.clone()),
                    _ => {}
                }
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(
            markdown_parts.len(),
            2,
            "the row's settled part and the delta's fresh part stay distinct: {markdown_parts:?}"
        );
        let settled_part = markdown_parts[0].as_str();
        assert_ne!(
            delta_part_ids.first().map(String::as_str),
            Some(settled_part),
            "the delta must not append into the settled row's part"
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

    /// The network approval card carries the two amendment options on top
    /// of the bare pair; every other tool's card stays at exactly
    /// allow-once/deny. The ids ARE the verdict vocabulary the gateway maps
    /// back.
    #[test]
    fn the_network_approval_card_offers_the_host_amendments() {
        for (tool_name, expected_ids) in [
            (
                manox_harness::sandbox::NETWORK_APPROVAL_TOOL,
                vec![
                    "allow_once",
                    "allow_host_session",
                    "allow_host_persistent",
                    "deny",
                ],
            ),
            ("Edit", vec!["allow_once", "deny"]),
        ] {
            let mut t = Translator::new();
            let mut out = Vec::new();
            for (seq, entry) in [
                kernel(json!({"type": "turn_start"})),
                kernel(json!({
                    "type": "approval", "id": "e-3", "kind": "request", "authId": "a-net",
                    "payload": {"toolName": tool_name},
                })),
            ]
            .into_iter()
            .enumerate()
            {
                out.extend(t.on_entry("c-1", "s-1", seq as u64, &entry));
            }
            let ids = out
                .iter()
                .find_map(|e| match &e.action {
                    StateAction::ChatToolCallReady(ready) => {
                        ready.options.as_ref().map(|options| {
                            options
                                .iter()
                                .map(|option| option.id.as_str())
                                .collect::<Vec<_>>()
                        })
                    }
                    _ => None,
                })
                .unwrap_or_default();
            assert_eq!(ids, expected_ids, "options for {tool_name}");
        }
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
        assert_eq!(
            request.message, None,
            "no detail anywhere means no request-level display message"
        );
    }

    /// The questions' `detail` markdown is the ask's support text — a plan
    /// review is just an ask whose detail is the plan. The AHP wire has no
    /// per-question detail slot, so it rides the request-level `message`:
    /// verbatim for one question, joined under the question headers when
    /// several carry content.
    #[test]
    fn ask_details_ride_the_request_level_message() {
        let single = kernel(json!({
            "type": "question",
            "id": "e9",
            "kind": "request",
            "authId": "call_plan",
            "payload": {
                "toolName": "AskUserQuestion",
                "input": {"questions": [{
                    "header": "Plan review",
                    "question": "Approve this plan?",
                    "detail": "# Plan\n\n- step one\n- step two",
                    "options": [{"label": "Approve"}, {"label": "Revise"}],
                }]},
            },
        }));
        let mut translator = Translator::new();
        let actions: Vec<_> = translator
            .on_entry("c-1", "s-1", 9, &single)
            .into_iter()
            .map(|e| e.action)
            .collect();
        let request = actions
            .iter()
            .find_map(|a| match a {
                StateAction::ChatInputRequested(r) => Some(&r.request),
                _ => None,
            })
            .expect("the ask reaches the client");
        assert_eq!(
            request.message.as_deref(),
            Some("# Plan\n\n- step one\n- step two"),
            "a single detail rides verbatim"
        );

        let several = kernel(json!({
            "type": "question",
            "id": "e10",
            "kind": "request",
            "authId": "call_two",
            "payload": {
                "toolName": "AskUserQuestion",
                "input": {"questions": [
                    {"header": "Scope", "question": "Which scope?", "detail": "scope notes",
                     "options": [{"label": "A"}]},
                    {"header": "Style", "question": "Which style?",
                     "options": [{"label": "B"}]},
                    {"question": "Anything else?", "detail": "trailing notes",
                     "options": [{"label": "C"}]},
                ]},
            },
        }));
        let actions: Vec<_> = translator
            .on_entry("c-1", "s-1", 10, &several)
            .into_iter()
            .map(|e| e.action)
            .collect();
        let request = actions
            .iter()
            .find_map(|a| match a {
                StateAction::ChatInputRequested(r) => Some(&r.request),
                _ => None,
            })
            .expect("the ask reaches the client");
        assert_eq!(
            request.message.as_deref(),
            Some("## Scope\n\nscope notes\n\n---\n\ntrailing notes"),
            "several details join under their headers; a detail-less question contributes nothing"
        );
    }

    /// A turn that dies with a parked question and NO journalled decision —
    /// the crash-before-discard shape — must still replay to a settled card.
    /// An unanswered ask in an archived turn is unanswerable by construction
    /// (the ahp reducer settles input requests on the ACTIVE turn only), so
    /// the boundary owes the settlement itself.
    #[test]
    fn a_turn_close_settles_a_question_the_journal_left_unanswered() {
        let records = vec![
            kernel(json!({
                "type": "message",
                "id": "sentry-0",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "plan the release"}]},
            })),
            kernel(json!({"type": "turn_start", "id": "sentry-1"})),
            kernel(json!({
                "type": "question",
                "id": "sentry-2",
                "kind": "request",
                "authId": "call_ask",
                "payload": {
                    "toolName": "AskUserQuestion",
                    "input": {"questions": [{
                        "question": "版本号怎么定？", "header": "版本号", "multiSelect": false,
                        "options": [{"label": "v1.4.0", "description": "minor"}],
                    }]},
                },
            })),
            // No `question` decision row: the crash took it.
            kernel(json!({"type": "turn_finish", "id": "sentry-3",
                          "cancelled": true, "failed": false, "strandedSteerIds": []})),
        ];
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        for (seq, entry) in records.into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        let turn = state.turns.last().expect("the turn archived");
        let ask = turn
            .response_parts
            .iter()
            .find_map(|p| match p {
                ahp_types::state::ResponsePart::InputRequest(input) => Some(input),
                _ => None,
            })
            .expect("the ask part survives the archive");
        assert_eq!(ask.request.id, "call_ask");
        assert!(
            ask.response.is_some(),
            "the boundary settled the ask the journal never decided — an \
             unanswered part in an archived turn strands every client fold"
        );
    }

    /// A journalled decision retires the open ask, so the turn close must
    /// NOT settle it a second time: one completion per ask, in journal order.
    #[test]
    fn a_journalled_decision_keeps_the_boundary_settlement_single() {
        let records = vec![
            kernel(json!({
                "type": "message",
                "id": "sentry-0",
                "message": {"role": "user",
                            "content": [{"type": "text", "text": "plan the release"}]},
            })),
            kernel(json!({"type": "turn_start", "id": "sentry-1"})),
            kernel(json!({
                "type": "question",
                "id": "sentry-2",
                "kind": "request",
                "authId": "call_ask",
                "payload": {
                    "toolName": "AskUserQuestion",
                    "input": {"questions": [{
                        "question": "q", "header": "h", "multiSelect": false,
                    }]},
                },
            })),
            kernel(json!({
                "type": "question",
                "id": "sentry-3",
                "kind": "decision",
                "authId": "call_ask",
                "payload": {"toolName": "AskUserQuestion", "verdict": "cancelled"},
            })),
            kernel(json!({"type": "turn_finish", "id": "sentry-4",
                          "cancelled": true, "failed": false, "strandedSteerIds": []})),
        ];
        let mut translator = Translator::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        let mut completions = Vec::new();
        for (seq, entry) in records.into_iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, &entry) {
                if let StateAction::ChatInputCompleted(done) = &emitted.action {
                    completions.push(done.request_id.clone());
                }
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(
            completions,
            vec!["call_ask".to_string()],
            "exactly one settlement: the decision row, not a second from the turn close"
        );
        let turn = state.turns.last().expect("the turn archived");
        let ask = turn
            .response_parts
            .iter()
            .find_map(|p| match p {
                ahp_types::state::ResponsePart::InputRequest(input) => Some(input),
                _ => None,
            })
            .expect("the ask part survives the archive");
        assert!(ask.response.is_some(), "the decision settled the card");
    }
}

#[cfg(test)]
mod preview_edits_tests {
    use super::*;

    fn kernel(mut fields: Value) -> SessionTreeEntry {
        let mut base = json!({
            "id": "e-1",
            "parentId": None::<String>,
            "timestamp": "2026-10-09T00:00:00.000Z",
        });
        let object = base.as_object_mut().expect("base is an object");
        if let Some(extra) = fields.as_object_mut() {
            object.append(extra);
        }
        serde_json::from_value(base.clone())
            .unwrap_or_else(|e| panic!("kernel fixture failed: {e} — {base}"))
    }

    /// Unknowable directions carry None, not zero: a clipboard paste adds an
    /// unknown number of lines — `Some(0)` would render as "+0" (reads "no
    /// changes") on a card that does change lines the fold cannot count. The
    /// app's ±? face keys off exactly this.
    #[test]
    fn unknowable_directions_carry_none_not_zero() {
        // A swap (both directions known) plus a paste (insertion unknown).
        let preview = preview_edits(
            "Edit",
            Some(&json!({
                "path": "/src/p.rs",
                "patch": "[src/p.rs#1A2B3C]\nSWAP 1.=1:\nfixed\nPASTE.POST 3:",
            })),
        )
        .expect("the preview parses");
        let diff = preview.items[0].diff.as_ref().expect("stats present");
        assert_eq!(diff.added, None, "the paste's insertion is unknown");
        assert_eq!(diff.removed, Some(1), "the swap's span counts exactly");

        // A pure block deletion: the removed direction is unknown, the added
        // direction stays a known zero (the patch adds nothing).
        let preview = preview_edits(
            "Edit",
            Some(&json!({
                "path": "/src/q.rs",
                "patch": "[src/q.rs#1A2B3C]\nDEL.BLK 2",
            })),
        )
        .expect("the preview parses");
        let diff = preview.items[0].diff.as_ref().expect("stats present");
        assert_eq!(diff.removed, None, "the block span is unknown");
        assert_eq!(diff.added, Some(0), "nothing is inserted");

        // A whole-file REM: the deleted file's line count is file state the
        // fold never sees — same unknown as a block span, never a zero.
        let preview = preview_edits(
            "Edit",
            Some(&json!({
                "path": "/src/r.rs",
                "patch": "[src/r.rs#1A2B3C]\nREM",
            })),
        )
        .expect("the preview parses");
        let edit = &preview.items[0];
        assert!(
            edit.before.is_some() && edit.after.is_none(),
            "a REM is a deletion"
        );
        let diff = edit.diff.as_ref().expect("stats present");
        assert_eq!(diff.removed, None, "the file's line count is unknown");
        assert_eq!(diff.added, Some(0), "nothing is inserted");
    }

    /// The approval gate opens with the preview face derived from the call's
    /// own journaled parameters: an `Edit` patch becomes per-file edits with
    /// statically-counted spans, a `Write` becomes a creation (after side
    /// only), and the counts ride the diff stats.
    #[test]
    fn the_approval_gate_carries_the_edit_preview_face() {
        let mut t = Translator::new();
        let mut out = Vec::new();
        for (seq, entry) in [
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "tool_call", "id": "e-2", "callId": "t-1",
                "name": "Edit", "title": "edit src", "status": "pending-approval",
                "input": {"patch": "[src/a.rs#1A2B3C]\nSWAP 2.=3:\nfresh line"},
            })),
            kernel(json!({
                "type": "approval", "id": "e-3", "kind": "request", "authId": "a-1",
                "payload": {"toolName": "Edit", "toolCallId": "t-1"},
            })),
        ]
        .into_iter()
        .enumerate()
        {
            out.extend(t.on_entry("c-1", "s-1", seq as u64, &entry));
        }
        let edits = out
            .iter()
            .find_map(|e| {
                let v = serde_json::to_value(&e.action).ok()?;
                v.pointer("/request/toolCall/edits")
                    .cloned()
                    .filter(|edits| !edits.is_null())
            })
            .expect("the pending state carries the preview");
        assert_eq!(edits["items"][0]["before"]["uri"], "file:///src/a.rs");
        assert_eq!(edits["items"][0]["after"]["uri"], "file:///src/a.rs");
        assert_eq!(edits["items"][0]["diff"]["added"], 1);
        assert_eq!(edits["items"][0]["diff"]["removed"], 2);

        // A creation is the after side alone.
        let mut t = Translator::new();
        let mut out = Vec::new();
        for (seq, entry) in [
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "tool_call", "id": "e-2", "callId": "t-2",
                "name": "Write", "title": "write new", "status": "pending-approval",
                "input": {"path": "/src/new.rs", "content": "one\ntwo\nthree"},
            })),
            kernel(json!({
                "type": "approval", "id": "e-3", "kind": "request", "authId": "a-2",
                "payload": {"toolName": "Write", "toolCallId": "t-2"},
            })),
        ]
        .into_iter()
        .enumerate()
        {
            out.extend(t.on_entry("c-1", "s-1", seq as u64, &entry));
        }
        let edits = out
            .iter()
            .find_map(|e| {
                let v = serde_json::to_value(&e.action).ok()?;
                v.pointer("/request/toolCall/edits")
                    .cloned()
                    .filter(|edits| !edits.is_null())
            })
            .expect("the write gate carries the preview");
        assert!(
            edits["items"][0]["before"].is_null(),
            "a creation has no before side"
        );
        assert_eq!(edits["items"][0]["after"]["uri"], "file:///src/new.rs");
        assert_eq!(edits["items"][0]["diff"]["added"], 3);
        assert!(
            edits["items"][0]["diff"]["removed"].is_null(),
            "a Write may overwrite an existing file; removed is unknowable"
        );

        // A tool with no file face previews nothing — no preview beats a wrong
        // one.
        let preview = preview_edits("bash", Some(&json!({"command": "ls"})));
        assert!(preview.is_none());
    }
}

#[cfg(test)]
mod annotations_tests {
    use super::*;

    fn kernel(mut fields: Value) -> SessionTreeEntry {
        let mut base = json!({
            "id": "e-1",
            "parentId": None::<String>,
            "timestamp": "2026-10-09T00:00:00.000Z",
        });
        let object = base.as_object_mut().expect("base is an object");
        if let Some(extra) = fields.as_object_mut() {
            object.append(extra);
        }
        serde_json::from_value(base.clone())
            .unwrap_or_else(|e| panic!("kernel fixture failed: {e} — {base}"))
    }

    /// The journal's annotation rows project onto the annotations channel
    /// and fold through the SDK reducer: latest-wins per id, removal
    /// collapses, exactly the state a subscriber seeds from.
    #[test]
    fn annotation_rows_fold_into_the_annotations_channel() {
        let annotation = json!({
            "id": "note-1",
            "origin": {"session": "ahp-session:/s-1"},
            "resource": "file:///src/a.rs",
            "resolved": false,
            "entries": [
                {"id": "en-1", "text": "check the anchor drift here"},
                {"id": "en-2", "text": {"markdown": "**and** the guard"}},
            ],
        });
        let mut t = Translator::new();
        let mut state = manox_ahp::channels::annotations::initial();
        let channel = manox_ahp::channels::annotations::uri("s-1");
        for (seq, entry) in [
            kernel(json!({"type": "annotation_set", "annotation": annotation.clone()})),
            kernel(json!({"type": "annotation_removed", "id": "e-2", "annotationId": "note-1"})),
        ]
        .into_iter()
        .enumerate()
        {
            for emitted in t.on_entry("c-1", "s-1", seq as u64, &entry) {
                assert_eq!(emitted.channel, channel, "annotations ride their channel");
                ahp::reducers::apply_action_to_annotations(&mut state, &emitted.action);
            }
        }
        assert!(
            state.annotations.is_empty(),
            "the removal collapsed the annotation: {:?}",
            state.annotations
        );

        // Re-set without the removal: one annotation, two entries, verbatim.
        let mut t = Translator::new();
        let mut state = manox_ahp::channels::annotations::initial();
        for emitted in t.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({"type": "annotation_set", "annotation": annotation.clone()})),
        ) {
            ahp::reducers::apply_action_to_annotations(&mut state, &emitted.action);
        }
        assert_eq!(state.annotations.len(), 1);
        assert_eq!(state.annotations[0].id, "note-1");
        assert_eq!(state.annotations[0].entries.len(), 2);
    }

    /// A run/started row with a tool call id opens the background-work
    /// pointer AND attaches the spawning tool call's forward pointer — a
    /// `ToolResultSubagentContent` naming the child chat — so the card, the
    /// row, and the drill-down correlate by spec vocabulary, with no
    /// private payload riding either.
    #[test]
    fn run_started_attaches_the_subagent_pointer_to_the_owning_tool_call() {
        let mut t = Translator::new();
        let out = t.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({
                "type": "run/started",
                "id": "e-1",
                "parentId": null,
                "timestamp": "1970-01-01T00:00:00Z",
                "runId": "sub-9",
                "kind": "Explore",
                "label": "survey the fold",
                "childSession": "sub-9",
                "toolCallId": "tu_1",
                "budgets": {},
                "depth": 1,
            })),
        );
        let chat = manox_ahp::channels::chat::uri("c-1");
        let mut work = 0;
        let mut pointer = None;
        for emitted in &out {
            if emitted.channel != chat {
                continue;
            }
            match &emitted.action {
                StateAction::ChatBackgroundWorkSet(_) => work += 1,
                StateAction::ChatToolCallContentChanged(action) => pointer = Some(action.clone()),
                _ => {}
            }
        }
        assert_eq!(work, 1, "the run opens exactly one background-work entry");
        let pointer = pointer.expect("the spawning tool call gets its forward pointer");
        assert_eq!(pointer.tool_call_id, "tu_1");
        assert_eq!(pointer.content.len(), 1);
        match &pointer.content[0] {
            ahp_types::state::ToolResultContent::Subagent(sub) => {
                assert_eq!(sub.resource, "ahp-chat:/sub-9");
                assert_eq!(sub.title, "survey the fold");
                assert_eq!(sub.agent_name.as_deref(), Some("Explore"));
            }
            other => panic!("unexpected content: {other:?}"),
        }
    }

    /// A UI note card (an annotation carrying the `x-manox` note payload)
    /// also lands on the chat face as a `systemNotification` part so a
    /// replayed transcript interleaves it in journal order; the kind and the
    /// tool anchor ride the part's `x-manox` meta.
    #[test]
    fn ui_note_annotations_also_ride_the_chat_face() {
        let annotation = json!({
            "id": "note-1",
            "origin": {"session": "ahp-session:/s-1"},
            "resource": "ahp-session:/s-1",
            "resolved": false,
            "entries": [{"id": "en-1", "text": "boom"}],
            "_meta": {"x-manox": {"uiNoteKind": "error", "toolCallId": "tu_1"}},
        });
        let mut t = Translator::new();
        let out = t.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({"type": "annotation_set", "annotation": annotation})),
        );
        let chat = manox_ahp::channels::chat::uri("c-1");
        let at = manox_ahp::channels::annotations::uri("s-1");
        let durable: Vec<_> = out.iter().filter(|e| e.channel == at).collect();
        let transcript: Vec<_> = out.iter().filter(|e| e.channel == chat).collect();
        assert_eq!(durable.len(), 1, "the annotations face keeps its action");
        let parts: Vec<_> = transcript
            .iter()
            .filter_map(|e| match &e.action {
                StateAction::ChatResponsePart(action) => Some(action),
                _ => None,
            })
            .collect();
        assert_eq!(
            parts.len(),
            1,
            "one card part on the transcript: {transcript:?}"
        );
        let ResponsePart::SystemNotification(note) = &parts[0].part else {
            panic!("expected a systemNotification part: {:?}", parts[0].part);
        };
        assert_eq!(note.content.as_text(), "boom");
        let meta = note.meta.as_ref().expect("the card carries its payload");
        let xmanox = &meta[ext::META_KEY];
        assert_eq!(xmanox["uiNoteKind"], "error");
        assert_eq!(xmanox["toolCallId"], "tu_1");
        assert_eq!(xmanox["annotationId"], "note-1");
    }

    /// A plain review annotation has no transcript face — the annotations
    /// channel stays its only landing.
    #[test]
    fn plain_review_annotations_stay_off_the_chat_face() {
        let annotation = json!({
            "id": "rev-1",
            "origin": {"session": "ahp-session:/s-1"},
            "resource": "file:///src/a.rs",
            "resolved": false,
            "entries": [{"id": "rev-1-e1", "text": "check the drift"}],
        });
        let mut t = Translator::new();
        let out = t.on_entry(
            "c-1",
            "s-1",
            0,
            &kernel(json!({"type": "annotation_set", "annotation": annotation})),
        );
        let chat = manox_ahp::channels::chat::uri("c-1");
        assert!(
            out.iter().all(|e| e.channel != chat),
            "no transcript emissions: {out:?}"
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
                kernel(json!({"type": "plan_update", "snapshot": {}})),
                ext_actions::PLAN_CHANGED,
                "x-manox-plan:/c-1",
            ),
            (
                kernel(json!({"type": "goal", "goal": {"text": "ship"}})),
                ext_actions::WORK_GOAL_CHANGED,
                "x-manox-work:/s-1",
            ),
            (
                kernel(json!({"type": "browser_suites", "suites": ["webexplore"]})),
                ext_actions::WORK_BROWSER_SUITES,
                "x-manox-work:/s-1",
            ),
            (
                kernel(json!({"type": "background_task", "snapshot": {"task_id": "bg_7"}})),
                ext_actions::WORK_BACKGROUND_TASKS,
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
        // Excluded, no fold arm to feed: BASELINE (host envelope), the
        // workspaces rows (declared, fold lives outside this bag).

        let mut state = XManoxState::default();
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
                plan: Some(json!({})),
                goal: Some(json!({"text": "ship"})),
                browser_suites: Some(vec!["webexplore".to_string()]),
                background_tasks: Some(
                    [("bg_7".to_string(), json!({"task_id": "bg_7"}))]
                        .into_iter()
                        .collect()
                ),
                pinned: Some(true),
                label: Some("x".to_string()),
                session_info: Some(json!({"name": "agent"})),
                leaf: Some("e-9".to_string()),
                alias: None,
                active_tools: Some(vec!["bash".to_string()]),
                order: Some(json!({"s-1": 1})),
            },
            "every extension-state field must be reachable from a live producer"
        );
    }
}

#[cfg(test)]
mod tool_input_publication_tests {
    use super::*;

    fn kernel(mut fields: Value) -> SessionTreeEntry {
        let mut base = json!({
            "id": "e-1",
            "parentId": None::<String>,
            "timestamp": "2026-10-21T00:00:00.000Z",
        });
        let object = base.as_object_mut().expect("base is an object");
        if let Some(extra) = fields.as_object_mut() {
            object.append(extra);
        }
        serde_json::from_value(base.clone())
            .unwrap_or_else(|e| panic!("kernel fixture failed: {e} — {base}"))
    }

    /// Fold the records through the shared translator + reducer, exactly as a
    /// subscriber's baseline does, on both channels a client folds.
    fn fold_both(records: &[SessionTreeEntry]) -> (ChatState, ahp_types::state::SessionState) {
        let mut translator = Translator::new();
        let mut chat = manox_ahp::channels::chat::initial("c-1");
        let mut session = manox_ahp::channels::session::initial("manox", None, None);
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                ahp::reducers::apply_action_to_chat(&mut chat, &emitted.action);
                ahp::reducers::apply_action_to_session(&mut session, &emitted.action);
            }
        }
        (chat, session)
    }

    fn fold(records: &[SessionTreeEntry]) -> ChatState {
        fold_both(records).0
    }

    /// The turn's pending card, for the text/fill assertions.
    fn pending_part(state: &ChatState) -> &ToolCallPendingConfirmationState {
        let parts = &state
            .active_turn
            .as_ref()
            .expect("an active turn")
            .response_parts;
        parts
            .iter()
            .find_map(|part| match part {
                ahp_types::state::ResponsePart::ToolCall(call) => match &call.tool_call {
                    ToolCallState::PendingConfirmation(pending) => Some(pending),
                    _ => None,
                },
                _ => None,
            })
            .expect("a pending confirmation part")
    }

    /// The turn's running part, for the catch-up assertions.
    fn running_part(state: &ChatState) -> &ahp_types::state::ToolCallRunningState {
        let parts = &state
            .active_turn
            .as_ref()
            .expect("an active turn")
            .response_parts;
        parts
            .iter()
            .find_map(|part| match part {
                ahp_types::state::ResponsePart::ToolCall(call) => match &call.tool_call {
                    ToolCallState::Running(running) => Some(running),
                    _ => None,
                },
                _ => None,
            })
            .expect("a running part")
    }

    /// The folded params of every tool call in the state, keyed by call id.
    fn folded_inputs(state: &ChatState) -> Vec<(String, Option<Value>)> {
        let mut inputs = Vec::new();
        let scan = |parts: &Vec<ahp_types::state::ResponsePart>, inputs: &mut Vec<_>| {
            for part in parts {
                if let ahp_types::state::ResponsePart::ToolCall(call) = part {
                    let input = match call.tool_call.clone() {
                        ToolCallState::Running(r) => r.tool_input,
                        ToolCallState::PendingConfirmation(p) => p.tool_input,
                        ToolCallState::Completed(c) => c.tool_input,
                        other => panic!("unexpected state: {other:?}"),
                    };
                    let input = match input {
                        Some(ToolInput::Inline(raw)) => Some(serde_json::from_str(&raw).unwrap()),
                        other => panic!("params must ride inline JSON: {other:?}"),
                    };
                    inputs.push((tool_call_handle(&call.tool_call).to_string(), input));
                }
            }
        };
        if let Some(active) = &state.active_turn {
            scan(&active.response_parts, &mut inputs);
        }
        for turn in &state.turns {
            scan(&turn.response_parts, &mut inputs);
        }
        inputs
    }

    fn params() -> Value {
        json!({"command": "cargo test -p manox-ahp-runtime"})
    }

    /// The plain run: the toolCall row's parameters must be on the wire by
    /// the time the call runs — the ready transition is where a delta's
    /// `partialInput` dies, so the ready action itself carries them.
    #[test]
    fn a_running_call_carries_its_parameters() {
        let state = fold(&[
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "tool_call", "id": "e-2", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "running",
                "input": params(),
            })),
        ]);
        assert_eq!(
            folded_inputs(&state),
            vec![("t-1".to_string(), Some(params()))],
            "the running state shows the command the model asked for"
        );
    }

    /// The settled call keeps the parameters: the completion preserves what
    /// the ready action landed.
    #[test]
    fn a_completed_call_still_carries_its_parameters() {
        let state = fold(&[
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "tool_call", "id": "e-2", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "running",
                "input": params(),
            })),
            kernel(json!({
                "type": "tool_result", "id": "e-3", "callId": "t-1",
                "output": "ok", "isError": false,
            })),
        ]);
        assert_eq!(
            folded_inputs(&state),
            vec![("t-1".to_string(), Some(params()))]
        );
    }

    /// The common approval order — the call row, then the gate: the pending
    /// card carries the parameters (what the user is asked to approve), and
    /// the verdict's running state keeps them.
    #[test]
    fn an_approval_after_the_call_row_carries_the_parameters_through_the_verdict() {
        let state = fold(&[
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "tool_call", "id": "e-2", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "pending-approval",
                "input": params(),
            })),
            kernel(json!({
                "type": "approval", "id": "e-3", "kind": "request", "authId": "a-1",
                "payload": {"toolName": "Bash", "toolCallId": "t-1"},
            })),
            kernel(json!({
                "type": "approval", "id": "e-4", "kind": "decision", "authId": "a-1",
                "payload": {"toolCallId": "t-1", "verdict": "allow_once"},
            })),
        ]);
        assert_eq!(
            folded_inputs(&state),
            vec![("t-1".to_string(), Some(params()))]
        );
    }

    /// The gate opening before the call row (approval-first): the catch-up
    /// delivers the parameters once the row lands, and the verdict keeps
    /// them into the running state.
    #[test]
    fn the_approval_first_order_catches_up_when_the_call_row_lands() {
        let state = fold(&[
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "approval", "id": "e-2", "kind": "request", "authId": "a-1",
                "payload": {"toolName": "Bash", "toolCallId": "t-1"},
            })),
            kernel(json!({
                "type": "tool_call", "id": "e-3", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "pending-approval",
                "input": params(),
            })),
            kernel(json!({
                "type": "approval", "id": "e-4", "kind": "decision", "authId": "a-1",
                "payload": {"toolCallId": "t-1", "verdict": "allow_once"},
            })),
            kernel(json!({
                "type": "tool_result", "id": "e-5", "callId": "t-1",
                "output": "ok", "isError": false,
            })),
        ]);
        assert_eq!(
            folded_inputs(&state),
            vec![("t-1".to_string(), Some(params()))]
        );
    }

    /// Status-update rows repeating the same parameters never re-publish
    /// them: one ready emission carries the payload, the rest are plain.
    #[test]
    fn repeated_parameter_rows_publish_once() {
        let mut translator = Translator::new();
        let mut readies_with_input = 0usize;
        let mut state = manox_ahp::channels::chat::initial("c-1");
        let records = [
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "tool_call", "id": "e-2", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "running",
                "input": params(),
            })),
            kernel(json!({
                "type": "tool_call", "id": "e-3", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "running",
                "input": params(),
            })),
            kernel(json!({
                "type": "tool_result", "id": "e-4", "callId": "t-1",
                "output": "ok", "isError": false,
            })),
        ];
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                if let StateAction::ChatToolCallReady(ready) = &emitted.action
                    && ready.tool_input.is_some()
                {
                    readies_with_input += 1;
                }
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(readies_with_input, 1, "one publication, not one per row");
        assert_eq!(
            folded_inputs(&state),
            vec![("t-1".to_string(), Some(params()))]
        );
    }

    /// The approval-first catch-up must not rewrite the text the user is
    /// reading: the gate's prompt survives the row that teaches the book its
    /// title, and the row fills the intention the gate could not name.
    #[test]
    fn the_approval_first_catch_up_keeps_the_confirmation_text() {
        let records = [
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "approval", "id": "e-2", "kind": "request", "authId": "a-1",
                "payload": {"toolName": "Bash", "toolCallId": "t-1"},
            })),
            kernel(json!({
                "type": "tool_call", "id": "e-3", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "pending-approval",
                "input": params(),
            })),
        ];
        let gate = fold(&records[..2]);
        let prompt = pending_part(&gate).invocation_message.clone();
        assert_eq!(
            prompt,
            StringOrMarkdown::Plain("Bash — awaiting confirmation".into())
        );
        let state = fold(&records);
        let pending = pending_part(&state);
        assert_eq!(
            pending.invocation_message, prompt,
            "the catch-up restates the gate's own prompt"
        );
        assert_eq!(
            pending.intention.as_deref(),
            Some("test"),
            "the row's title fills the blank intention"
        );
    }

    /// The session mirror is a surface a client may read instead of the chat
    /// card; the same catch-up must teach it the parameters.
    #[test]
    fn the_pending_mirror_learns_the_parameters_when_the_row_lands() {
        let records = [
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "approval", "id": "e-2", "kind": "request", "authId": "a-1",
                "payload": {"toolName": "Bash", "toolCallId": "t-1"},
            })),
            kernel(json!({
                "type": "tool_call", "id": "e-3", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "pending-approval",
                "input": params(),
            })),
        ];
        let (_, session) = fold_both(&records);
        let mirror = session
            .input_needed
            .as_ref()
            .and_then(|requests| {
                requests.iter().find_map(|request| match request {
                    SessionInputRequest::ToolConfirmation(confirmation) => {
                        match &confirmation.tool_call {
                            ToolCallConfirmationState::PendingConfirmation(pending) => {
                                Some(pending)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                })
            })
            .expect("the gate's mirror");
        assert_eq!(
            mirror.tool_input,
            Some(ToolInput::Inline(params().to_string()))
        );
    }

    /// A parameter row that lands after the verdict and after output must keep
    /// the running part's approval reason and streamed output: the catch-up
    /// restates both instead of rebuilding the part empty.
    #[test]
    fn a_late_parameter_row_keeps_the_verdict_and_the_output() {
        let records = [
            kernel(json!({"type": "turn_start"})),
            kernel(json!({
                "type": "approval", "id": "e-2", "kind": "request", "authId": "a-1",
                "payload": {"toolName": "Bash", "toolCallId": "t-1"},
            })),
            kernel(json!({
                "type": "approval", "id": "e-3", "kind": "decision", "authId": "a-1",
                "payload": {"toolCallId": "t-1", "verdict": "allow_once"},
            })),
            kernel(json!({
                "type": "tool_call", "id": "e-4", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "running",
            })),
            kernel(json!({
                "type": "tool_output_chunk", "id": "e-5", "callId": "t-1", "chunk": "hello",
            })),
            kernel(json!({
                "type": "tool_call", "id": "e-6", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "running",
                "input": params(),
            })),
        ];
        let state = fold(&records);
        let running = running_part(&state);
        assert_eq!(
            running.confirmed,
            ToolCallConfirmationReason::UserAction,
            "the verdict survives the row that lands after it"
        );
        assert_eq!(
            running.tool_input,
            Some(ToolInput::Inline(params().to_string()))
        );
        assert_eq!(
            running.content,
            Some(vec![ToolResultContent::Text(ToolResultTextContent {
                text: "hello".into()
            })]),
            "the catch-up restores the streamed output"
        );
        assert_eq!(
            running.invocation_message,
            StringOrMarkdown::Plain("test".into())
        );
    }
}

/// The subagent roster survives a translator reset: a bridge restart or lag
/// resync skips the `run/started` rows below its resume point, so the roster
/// must be primed from the host's fold for the later `run/settled` to remove
/// the entries the client still holds.
#[cfg(test)]
mod subagent_roster_tests {
    use super::*;

    fn kernel(mut fields: Value) -> SessionTreeEntry {
        let mut base = json!({
            "id": "e-1",
            "parentId": None::<String>,
            "timestamp": "2026-10-09T00:00:00.000Z",
        });
        let object = base.as_object_mut().expect("base is an object");
        if let Some(extra) = fields.as_object_mut() {
            object.append(extra);
        }
        serde_json::from_value(base.clone())
            .unwrap_or_else(|e| panic!("kernel fixture failed: {e} — {base}"))
    }

    fn started(run_id: &str) -> SessionTreeEntry {
        kernel(json!({
            "type": "run/started",
            "id": "e-2",
            "runId": run_id,
            "kind": "Explore",
            "label": "survey the fold",
            "childSession": run_id,
            "toolCallId": "",
            "budgets": {},
            "depth": 1,
        }))
    }

    fn settled(run_id: &str) -> SessionTreeEntry {
        kernel(json!({
            "type": "run/settled",
            "id": "e-9",
            "runId": run_id,
            "settlement": "completed",
            "verdict": "done",
            "turns": 1,
            "toolCalls": 1,
        }))
    }

    fn background_work_edges(out: &[Emitted]) -> (usize, usize) {
        let chat = manox_ahp::channels::chat::uri("c-1");
        let mut sets = 0;
        let mut removed = 0;
        for emitted in out {
            if emitted.channel != chat {
                continue;
            }
            match &emitted.action {
                StateAction::ChatBackgroundWorkSet(_) => sets += 1,
                StateAction::ChatBackgroundWorkRemoved(_) => removed += 1,
                _ => {}
            }
        }
        (sets, removed)
    }

    /// A primed roster settles: the removal goes out with no `run/started`
    /// in the translator's past — the shape of a bridge restart whose resume
    /// point sits above the start row.
    #[test]
    fn a_primed_roster_settles_without_a_replayed_start_row() {
        let mut t = Translator::new();
        t.prime_subagent_roster(["sub-9".to_string()]);
        let out = t.on_entry("c-1", "s-1", 0, &settled("sub-9"));
        let (sets, removed) = background_work_edges(&out);
        assert_eq!(sets, 0, "priming publishes no edge: {out:?}");
        assert_eq!(removed, 1, "the settle removes the client's entry: {out:?}");
    }

    /// A primed roster absorbs the replayed start row: the entry is already
    /// listed, so no second Set edge goes out — the client already holds it.
    #[test]
    fn a_primed_roster_does_not_republish_a_replayed_start_row() {
        let mut t = Translator::new();
        t.prime_subagent_roster(["sub-9".to_string()]);
        let out = t.on_entry("c-1", "s-1", 0, &started("sub-9"));
        let (sets, removed) = background_work_edges(&out);
        assert_eq!(sets, 0, "the entry is already listed: {out:?}");
        assert_eq!(removed, 0);
        // The replayed run still settles to exactly one removal.
        let out = t.on_entry("c-1", "s-1", 1, &settled("sub-9"));
        let (sets, removed) = background_work_edges(&out);
        assert_eq!(sets, 0);
        assert_eq!(removed, 1, "the settle still lands exactly once: {out:?}");
    }

    /// Without priming the settle stays silent — the pre-fix behavior kept
    /// as the honest baseline: an unlisted id has nothing to remove on the
    /// client, which is exactly why the roster must be primed.
    #[test]
    fn an_unprimed_roster_settles_to_nothing() {
        let mut t = Translator::new();
        let out = t.on_entry("c-1", "s-1", 0, &settled("sub-9"));
        let (sets, removed) = background_work_edges(&out);
        assert_eq!(sets, 0);
        assert_eq!(removed, 0, "an unlisted settle publishes no removal");
    }
}

/// The session-mutable config seat: the journal rows are the durable
/// authority for model / reasoningEffort / accessMode, so a fold — a fresh
/// subscribe, a resubscribe, a host restart, any replay of the same chain —
/// must seat all three values. This is the L10 gate (`快照 == fold(重放)`)
/// covering the config model.
#[cfg(test)]
mod config_seat_tests {
    use super::*;

    /// The session state a subscriber starts from: the declared schema with
    /// no values — exactly what `session_state` seeds (`empty_config`), the
    /// seat the reducer merges `session/configChanged` values into.
    fn seated_session() -> ahp_types::state::SessionState {
        manox_ahp::channels::session::initial("manox", None, Some(crate::ahp::empty_config()))
    }

    fn fold_config(records: &[SessionTreeEntry]) -> serde_json::Map<String, Value> {
        let mut translator = Translator::new();
        let mut session = seated_session();
        for (seq, entry) in records.iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                ahp::reducers::apply_action_to_session(&mut session, &emitted.action);
            }
        }
        session
            .config
            .expect("the schema seat survives the fold")
            .values
            .into_iter()
            .collect()
    }

    /// The user-symptom regression: the three picks a session carried land
    /// as rows, and every fold of that chain seats all three. On the
    /// pre-row authority this is red twice over — an `access_mode_change`
    /// row is not even in the vocabulary (the journal refuses it), and the
    /// fold had no arm to seat it, so a resubscribed client watched the
    /// model chip and the access menu drift back to the defaults.
    #[test]
    fn the_config_rows_seat_all_three_keys_on_every_fold() {
        let records = [
            kernel(json!({
                "type": "model_change",
                "provider": "anthropic",
                "modelId": "claude-opus-4-7",
            })),
            kernel(json!({
                "type": "thinking_level_change",
                "thinkingLevel": "max",
            })),
            kernel(json!({
                "type": "access_mode_change",
                "mode": "full-access",
            })),
        ];
        let expected = |key: &str, value: &str| {
            (
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            )
        };
        let want = [
            expected(config_keys::MODEL, "anthropic/claude-opus-4-7"),
            expected(config_keys::REASONING_EFFORT, "max"),
            expected(config_keys::ACCESS_MODE, "full-access"),
        ];

        let first = fold_config(&records);
        for (key, value) in &want {
            assert_eq!(
                first.get(key),
                Some(value),
                "the first subscribe seats `{key}` from the journal rows"
            );
        }

        // Resubscribe: a fresh fold of the same chain (a new translator, a
        // fresh seat) — the unsubscribe→subscribe path replays the journal
        // and must converge on the same three values, not the defaults.
        let resubscribed = fold_config(&records);
        for (key, value) in &want {
            assert_eq!(
                resubscribed.get(key),
                Some(value),
                "the resubscribe seats `{key}` from the journal rows"
            );
        }
    }

    /// Last-wins per key, in chain order: a later row overrides an earlier
    /// pick in the seat, the same fold the engine's restore reads.
    #[test]
    fn a_later_config_row_wins_in_the_seat() {
        let values = fold_config(&[
            kernel(json!({
                "type": "access_mode_change",
                "mode": "full-access",
            })),
            kernel(json!({
                "type": "access_mode_change",
                "mode": "sandbox",
            })),
        ]);
        assert_eq!(
            values.get(config_keys::ACCESS_MODE),
            Some(&serde_json::Value::String("sandbox".to_string())),
        );
    }
}

#[cfg(test)]
mod tool_output_content_tests {
    use super::*;

    fn kernel(mut fields: Value) -> SessionTreeEntry {
        let mut base = json!({
            "id": "e-1",
            "parentId": None::<String>,
            "timestamp": "2026-10-21T00:00:00.000Z",
        });
        let object = base.as_object_mut().expect("base is an object");
        if let Some(extra) = fields.as_object_mut() {
            object.append(extra);
        }
        serde_json::from_value(base.clone())
            .unwrap_or_else(|e| panic!("kernel fixture failed: {e} — {base}"))
    }

    fn records() -> Vec<SessionTreeEntry> {
        vec![
            kernel(json!({"type": "turn_start", "id": "e-1"})),
            kernel(json!({
                "type": "tool_call", "id": "e-2", "callId": "t-1",
                "name": "Bash", "title": "test", "status": "running",
            })),
            kernel(json!({
                "type": "tool_output_chunk", "id": "e-3", "callId": "t-1", "chunk": "a",
            })),
            kernel(json!({
                "type": "tool_output_chunk", "id": "e-4", "callId": "t-1", "chunk": "b",
            })),
            kernel(json!({
                "type": "tool_output_chunk", "id": "e-5", "callId": "t-1", "chunk": "c",
            })),
            kernel(json!({
                "type": "tool_result", "id": "e-6", "callId": "t-1",
                "output": "abc", "isError": false,
            })),
        ]
    }

    /// The partial text one `chat/toolCallContentChanged` emission carried.
    fn content_text(changed: &ChatToolCallContentChangedAction) -> String {
        changed
            .content
            .iter()
            .map(|block| match block {
                ToolResultContent::Text(text) => text.text.clone(),
                other => panic!("unexpected content block: {other:?}"),
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// The client reducer REPLACES a running call's content array on every
    /// `chat/toolCallContentChanged`, so each emission must carry the whole
    /// partial text so far — a latest-chunk emission would leave the fold
    /// holding only the newest fragment.
    #[test]
    fn content_changed_carries_the_accumulated_output_not_the_latest_chunk() {
        let mut translator = Translator::new();
        let mut texts = Vec::new();
        let mut state = manox_ahp::channels::chat::initial("c-1");
        for (seq, entry) in records().iter().enumerate() {
            for emitted in translator.on_entry("c-1", "s-1", seq as u64, entry) {
                if let StateAction::ChatToolCallContentChanged(changed) = &emitted.action {
                    texts.push(content_text(changed));
                }
                ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
            }
        }
        assert_eq!(texts, vec!["a", "ab", "abc"]);
        let settled = match tool_call_part(&state) {
            ToolCallState::Completed(completed) => completed,
            other => panic!("the result row settles the call: {other:?}"),
        };
        assert_eq!(
            settled.content,
            Some(vec![ToolResultContent::Text(ToolResultTextContent {
                text: "abc".into()
            })]),
            "the settled call keeps the full content its emissions accumulated"
        );
    }

    /// The accumulation lives in the projection's own bookkeeping, so a fresh
    /// translator folding the same journal must reproduce the emission stream
    /// byte for byte — that equality is what makes a re-subscribe's baseline
    /// and a live subscriber's stream agree.
    #[test]
    fn replaying_the_journal_reproduces_the_content_emissions_byte_for_byte() {
        let mut live = Translator::new();
        let mut replay = Translator::new();
        let mut live_emissions = Vec::new();
        let mut replayed_emissions = Vec::new();
        for (seq, entry) in records().iter().enumerate() {
            live_emissions.extend(live.on_entry("c-1", "s-1", seq as u64, entry));
            replayed_emissions.extend(replay.on_entry("c-1", "s-1", seq as u64, entry));
        }
        assert_eq!(live_emissions, replayed_emissions);
        let content: Vec<_> = replayed_emissions
            .iter()
            .filter_map(|emitted| match &emitted.action {
                StateAction::ChatToolCallContentChanged(changed) => Some(content_text(changed)),
                _ => None,
            })
            .collect();
        assert_eq!(content, vec!["a", "ab", "abc"]);
    }

    /// The turn's tool-call part, for the fold-state assertions.
    fn tool_call_part(state: &ChatState) -> &ahp_types::state::ToolCallState {
        let parts = &state
            .active_turn
            .as_ref()
            .expect("an active turn")
            .response_parts;
        parts
            .iter()
            .find_map(|part| match part {
                ahp_types::state::ResponsePart::ToolCall(call) => Some(&call.tool_call),
                _ => None,
            })
            .expect("a tool call part")
    }
}
