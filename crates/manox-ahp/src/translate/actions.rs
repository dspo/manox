//! Journal entries → AHP actions.
//!
//! The durable journal is the only authority (§C, L3/L4); this module turns each
//! accepted entry into the AHP actions that carry the same fact, paired with the
//! channel they belong on. [`crate::translate::target_of`] fixes an entry's
//! *primary* channel and is the compile-time totality gate; an arm here may add a
//! follow-up action on a second channel when the protocol mirrors one fact in two
//! places — a tool confirmation is a chat state machine *and* a session-level
//! `inputNeeded` entry.
//!
//! # Shape of the mapping
//!
//! - **Transcript rows** (`message`, `uiNote`, `custom`, `customMessage`) become
//!   ordered response parts. A `user` row rides the next `chat/turnStarted` —
//!   `_meta["x-manox"].originRpc` is what retires the client's optimistic echo
//!   (L7) — and is visible as a queued pending message until then, so no durable
//!   row is ever invisible.
//! - **Streaming** follows AHP's create-then-append contract: the first delta of
//!   a run emits `chat/responsePart` creating an empty markdown / reasoning part,
//!   later deltas append with `chat/delta` / `chat/reasoning`. A run is
//!   *contiguous* text (or thinking): the other stream, a tool call, a
//!   notification or a turn boundary closes it, so the next delta opens a new
//!   part. That state machine is why this type exists.
//! - **Tool calls** map the journal's status strings onto AHP's lifecycle
//!   ([`status_phase`]); approvals drive the `pending-confirmation ⇄ running /
//!   cancelled` transitions. Fail-closed approval policy is the runtime's — this
//!   is translation only.
//! - **What AHP cannot represent** goes to the x-manox surface declared in
//!   [`crate::ext`] (extension channels for plan / work / metrics, extension
//!   actions on standard channels for session facts with no field — pin, label,
//!   leaf cursor), or, when the fact belongs in the transcript's order, to a
//!   `systemNotification` response part. Each arm names which route it takes.
//!
//! # Determinism
//!
//! Every identity minted here derives from a journal entry id: turns
//! (`t-<turnStart id>`), parts (`p-<first delta of the run id>`), pending
//! messages (`m-<row id>`). Replaying the same journal yields the same ids in the
//! same order — L10's `snapshot == fold(replay)`, and why a restarted host can
//! seed from one fold and keep advancing it.
//!
//! # Duplicated facts
//!
//! Streaming deltas *and* the settled assistant row both carry the reply text;
//! `ToolCall{success}` and the following `ToolResult` both end a call. The rule is
//! first-writer-wins, the other silent, so the fold holds each fact once. The arms
//! say which.

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
use manox_journal::{JournalWireEntry, JournalWireEvent, UsagePayload, plan_review_request_id};
use serde_json::{Value, json};

use crate::ext;
use crate::translate::Target;

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
        crate::wire::action_tag(&self.action)
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
/// Grouped rather than passed positionally: the row vocabulary is the journal's,
/// and a struct keeps the action builder's signature about its job (build the
/// actions for one row) instead of about the journal's column list.
struct MessageRow<'a> {
    role: &'a str,
    content: &'a [Value],
    usage: Option<&'a UsagePayload>,
    origin_rpc: Option<&'a str>,
    display: Option<bool>,
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

    /// Translate one accepted journal entry into the actions it produces.
    ///
    /// An empty result is a decision, not an oversight, and each case is named
    /// where it arises: a `tool` transcript row whose `toolResult` already settled
    /// the call, a verdict for a turn that closed, an idle `stop` with nothing
    /// running. `tests/translation_convergence.rs` drives every vocabulary kind and
    /// asserts each one reaches a channel.
    pub fn on_entry(
        &mut self,
        chat_id: &str,
        session_id: &str,
        entry: &JournalWireEntry,
    ) -> Vec<Emitted> {
        let at = crate::translate::target_of(&entry.event).uri(chat_id, session_id);
        let chat = Target::Chat.uri(chat_id, session_id);
        let session = Target::Session.uri(chat_id, session_id);
        let thread = Target::Thread.uri(chat_id, session_id);
        let plan = Target::Plan.uri(chat_id, session_id);
        let mut out = Vec::new();
        match &entry.event {
            // ── transcript ───────────────────────────────────────────────
            JournalWireEvent::Message {
                role,
                content,
                usage,
                origin_rpc,
                display,
            } => self.on_message(
                &chat,
                entry,
                MessageRow {
                    role,
                    content,
                    usage: usage.as_ref(),
                    origin_rpc: origin_rpc.as_deref(),
                    display: *display,
                },
                &mut out,
            ),
            JournalWireEvent::UiNote { kind, data } => {
                let text = note_text(data).unwrap_or_else(|| kind.clone());
                self.push_note(
                    &chat,
                    entry,
                    text,
                    json!({"uiNote": {"kind": kind, "data": data}}),
                    &mut out,
                );
            }
            JournalWireEvent::Custom { custom_type, data } => {
                let text = note_text(data).unwrap_or_else(|| custom_type.clone());
                self.push_note(
                    &chat,
                    entry,
                    text,
                    json!({"custom": {"customType": custom_type, "data": data}}),
                    &mut out,
                );
            }
            JournalWireEvent::CustomMessage {
                custom_type,
                content,
                display,
            } => {
                let text = blocks_text(content);
                let fallback = custom_type.clone();
                let body = if text.is_empty() { fallback } else { text };
                self.push_note(
                    &chat,
                    entry,
                    body,
                    json!({"customMessage": {
                        "customType": custom_type,
                        "display": display,
                        "blocks": content,
                    }}),
                    &mut out,
                );
            }
            // ── turn lifecycle ───────────────────────────────────────────
            JournalWireEvent::TurnStart { .. } => self.on_turn_start(&chat, entry, &mut out),
            JournalWireEvent::TurnFinish {
                cancelled,
                failed,
                stranded_steer_ids,
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
                self.close_turn(&chat, entry, outcome, &mut out);
            }
            // A `stop` row is per-assistant-message metadata — the model's own
            // stop reason (end_turn / tool_use / max_tokens / refusal /
            // cancelled) — never turn lifecycle: `tool_use` and `max_tokens`
            // are the loop's continue edges (the tool / continuation rows
            // follow them), and a turn end always carries its `turnFinish`
            // right behind the last stop. Closing the turn here split every
            // agentic turn per tool round and minted a "stopped: …" notice
            // per round.
            JournalWireEvent::Stop { .. } => {}
            JournalWireEvent::Error { message } => self.close_turn(
                &chat,
                entry,
                Outcome::Failed(ErrorInfo {
                    error_type: "agent_error".to_string(),
                    message: message.clone(),
                    stack: None,
                    meta: None,
                }),
                &mut out,
            ),
            JournalWireEvent::Retry {
                attempt,
                max_attempts,
                delay_secs,
                reason,
            } => {
                // AHP has no retry concept: a provider retry is harness chatter
                // inside a live turn, so it surfaces as a notification part in the
                // transcript's order, with the counters in `_meta`.
                self.push_note(
                    &chat,
                    entry,
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
            JournalWireEvent::AgentTextDelta { s } => {
                self.on_delta(&chat, entry, Run::Text, s, &mut out);
            }
            JournalWireEvent::AgentThinkingDelta { s } => {
                self.on_delta(&chat, entry, Run::Thinking, s, &mut out);
            }
            // ── tool activity ────────────────────────────────────────────
            JournalWireEvent::ToolCall {
                call_id,
                name,
                title,
                status,
                input,
            } => self.on_tool_call(&chat, entry, call_id, name, title, status, input, &mut out),
            JournalWireEvent::ToolResult {
                call_id,
                output,
                is_error,
            } => self.on_tool_result(&chat, entry, call_id, output, *is_error, &mut out),
            JournalWireEvent::ToolOutputChunk { call_id, chunk } => {
                self.on_tool_output(&chat, entry, call_id, chunk, &mut out);
            }
            // ── adjudications ────────────────────────────────────────────
            JournalWireEvent::Approval {
                kind,
                auth_id,
                tool_name,
                tool_call_id,
                verdict,
                reason,
            } => self.on_approval(
                &chat,
                &session,
                entry,
                kind,
                auth_id,
                tool_name.as_deref(),
                tool_call_id.as_deref(),
                verdict.as_deref(),
                reason.as_deref(),
                &mut out,
            ),
            JournalWireEvent::Question {
                kind,
                auth_id,
                tool_name,
                verdict,
                reason,
                input,
                ..
            } => self.on_question(
                &chat,
                entry,
                AskRow {
                    kind,
                    auth_id,
                    tool_name: tool_name.as_deref(),
                    verdict: verdict.as_deref(),
                    reason: reason.as_deref(),
                    input: input.as_ref(),
                },
                &mut out,
            ),
            // ── sub-agent activity ───────────────────────────────────────
            JournalWireEvent::SubagentChild { .. } => {
                // One child session's lifecycle event, in the parent
                // transcript's order — and nothing to push for it: the
                // sub-agent tree is `x-manox-work` state (the Progress rows
                // below maintain it), and a notice per event flooded the
                // transcript with a card per spawn/exit the moment the bridge
                // streamed again.
            }
            JournalWireEvent::SubagentProgress {
                agent_id,
                agent_type,
                tool_uses,
                latest_activity,
                status,
            } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::WORK_SUBAGENTS,
                    json!({"agentId": agent_id, "agentType": agent_type,
                           "toolUses": tool_uses, "latestActivity": latest_activity,
                           "status": status}),
                ),
            )),
            JournalWireEvent::ActiveToolsChange { tools } => out.push(Emitted::new(
                &at,
                extension_action(ext::actions::WORK_ACTIVE_TOOLS, json!({"tools": tools})),
            )),
            // ── session state ────────────────────────────────────────────
            JournalWireEvent::Title { title } => out.push(Emitted::new(
                &session,
                StateAction::SessionTitleChanged(SessionTitleChangedAction {
                    title: title.clone(),
                }),
            )),
            JournalWireEvent::CwdChange { path } => {
                // AHP models working directories as a *granted set*; the effective
                // directory is not a protocol field, so the grant rides the
                // membership action and the effective value rides the config model.
                let directory = file_uri(path);
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
                    config_changed(json!({config_keys::WORKING_DIRECTORY: path})),
                ));
            }
            JournalWireEvent::ProjectChange { path } => out.push(Emitted::new(
                &session,
                config_changed(json!({config_keys::PROJECT: path})),
            )),
            JournalWireEvent::ModelChange { to, .. } => out.push(Emitted::new(
                &session,
                config_changed(json!({config_keys::MODEL: to.0.clone()})),
            )),
            JournalWireEvent::ReasoningEffortChange { effort } => out.push(Emitted::new(
                &session,
                config_changed(json!({config_keys::REASONING_EFFORT: effort})),
            )),
            JournalWireEvent::PermissionModeChange { mode } => out.push(Emitted::new(
                &session,
                config_changed(json!({config_keys::APPROVAL_MODE: mode})),
            )),
            JournalWireEvent::PinnedArchived { pinned, archived } => {
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
            JournalWireEvent::Label { label } => out.push(Emitted::new(
                &thread,
                extension_action(ext::actions::LABEL_CHANGED, json!({"label": label})),
            )),
            JournalWireEvent::SessionInfo { data } => out.push(Emitted::new(
                &thread,
                // The row rides nested under `data`: the fold reads that key,
                // and a bare flatten would scatter the row's fields across the
                // action envelope where no arm looks for them.
                extension_action(ext::actions::SESSION_INFO_CHANGED, json!({"data": data})),
            )),
            JournalWireEvent::Leaf { target_id } => out.push(Emitted::new(
                &thread,
                extension_action(ext::actions::LEAF_CHANGED, json!({"targetId": target_id})),
            )),
            // ── plan ─────────────────────────────────────────────────────
            JournalWireEvent::PlanModeChange { enabled } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::PLAN_MODE_CHANGED,
                    json!({"enabled": enabled, "pending": false}),
                ),
            )),
            JournalWireEvent::PlanModeRequest { enabled } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::PLAN_MODE_CHANGED,
                    json!({"enabled": enabled, "pending": true}),
                ),
            )),
            JournalWireEvent::PlanUpdate { snapshot } => out.push(Emitted::new(
                &at,
                extension_action(ext::actions::PLAN_CHANGED, json!({"snapshot": snapshot})),
            )),
            JournalWireEvent::PlanReview {
                state,
                plan_file,
                title,
                content,
                request_id: row_request_id,
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
                    // wrote after the field existed; translator memory covers
                    // rows written before it (the proposal replayed in this
                    // translator's own lifetime). A resolution neither names
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
                            seq = entry.seq,
                            "plan review resolved with no correlatable request id; no settlement emitted"
                        );
                    }
                } else {
                    let request_id = plan_review_request_id(&entry.id);
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
            JournalWireEvent::Goal { goal } => out.push(Emitted::new(
                &at,
                extension_action(ext::actions::WORK_GOAL_CHANGED, json!({"goal": goal})),
            )),
            JournalWireEvent::BrowserSuites { suites } => out.push(Emitted::new(
                &at,
                extension_action(ext::actions::WORK_BROWSER_SUITES, json!({"suites": suites})),
            )),
            JournalWireEvent::BackgroundTask { snapshot } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::WORK_BACKGROUND_TASKS,
                    json!({"snapshot": snapshot}),
                ),
            )),
            // ── metrics ──────────────────────────────────────────────────
            JournalWireEvent::Metrics { kind, data } => out.push(Emitted::new(
                &at,
                extension_action(
                    ext::actions::METRICS_CHANGED,
                    json!({"kind": kind, "data": data}),
                ),
            )),
            // ── compaction and branch summaries ──────────────────────────
            JournalWireEvent::Compaction {
                summary,
                messages_compacted,
                tokens_before,
                retained_tail,
                first_kept_entry_id,
            } => {
                // AHP has no "replace the transcript" action, so the fold keeps
                // every pre-compaction turn and this row is the visible seam: the
                // summary renders in order and `_meta` says where history was cut
                // (§B.5's gap). The spinner edge clears here too.
                out.push(Emitted::new(
                    &chat,
                    StateAction::ChatActivityChanged(ChatActivityChangedAction { activity: None }),
                ));
                mirror_activity(&chat, &session, None, &mut out);
                self.push_note(
                    &chat,
                    entry,
                    summary.clone(),
                    json!({"compaction": {
                        "messagesCompacted": messages_compacted,
                        "tokensBefore": tokens_before,
                        "retainedTail": retained_tail.len(),
                        "firstKeptEntryId": first_kept_entry_id,
                    }}),
                    &mut out,
                );
            }
            JournalWireEvent::CompactionStarted { tokens_before } => {
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
            JournalWireEvent::BranchSummary { text } => self.push_note(
                &chat,
                entry,
                text.clone(),
                json!({"branchSummary": {"text": text}}),
                &mut out,
            ),
        }
        out
    }

    /// The `message` row group.
    fn on_message(
        &mut self,
        chat: &str,
        entry: &JournalWireEntry,
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
    fn on_turn_start(&mut self, chat: &str, entry: &JournalWireEntry, out: &mut Vec<Emitted>) {
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
        entry: &JournalWireEntry,
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
        entry: &JournalWireEntry,
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
        entry: &JournalWireEntry,
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
        entry: &JournalWireEntry,
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
        entry: &JournalWireEntry,
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
        entry: &JournalWireEntry,
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
        entry: &JournalWireEntry,
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
        entry: &JournalWireEntry,
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
        entry: &JournalWireEntry,
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
    fn ensure_turn(&mut self, entry: &JournalWireEntry, chat: &str, out: &mut Vec<Emitted>) {
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
fn no_user_row(entry: &JournalWireEntry) -> Message {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, event: JournalWireEvent) -> JournalWireEntry {
        JournalWireEntry {
            seq: 0,
            id: id.to_string(),
            parent_id: None,
            timestamp: "2026-09-27T00:00:00.000Z".to_string(),
            event,
        }
    }

    #[test]
    fn a_set_activity_edge_mirrors_onto_the_session_channel_twice() {
        let mut translator = Translator::new();
        let emitted = translator.on_entry(
            "c-1",
            "s-1",
            &entry(
                "e-1",
                JournalWireEvent::CompactionStarted {
                    tokens_before: 4200,
                },
            ),
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
            &entry(
                "e-1",
                JournalWireEvent::Compaction {
                    summary: "done".into(),
                    messages_compacted: 3,
                    tokens_before: 10,
                    retained_tail: vec![],
                    first_kept_entry_id: None,
                },
            ),
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
            &entry(
                "e-1",
                JournalWireEvent::PlanReview {
                    state: "proposed".into(),
                    plan_file: Some("/plans/demo-plan.md".into()),
                    title: Some("Demo plan".into()),
                    content: Some("# Demo\n\n- step one".into()),
                    request_id: None,
                },
            ),
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
            &entry(
                "e-1",
                JournalWireEvent::PlanReview {
                    state: "proposed".into(),
                    plan_file: Some("/p.md".into()),
                    title: Some("T".into()),
                    content: Some("# T".into()),
                    request_id: None,
                },
            ),
        );
        let emitted = translator.on_entry(
            "c-1",
            "s-1",
            &entry(
                "e-2",
                JournalWireEvent::PlanReview {
                    state: "resolved".into(),
                    plan_file: None,
                    title: None,
                    content: None,
                    request_id: None,
                },
            ),
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
    /// request id settles in a translator that never saw the proposal (a
    /// bridge resuming above it, after a host restart). The payload names
    /// the plan with the proposal edge's `planUri` encoding.
    #[test]
    fn plan_review_resolution_survives_a_translator_restart() {
        let mut translator = Translator::new();
        let emitted = translator.on_entry(
            "c-1",
            "s-1",
            &entry(
                "e-9",
                JournalWireEvent::PlanReview {
                    state: "resolved".into(),
                    plan_file: Some("/p.md".into()),
                    title: None,
                    content: None,
                    request_id: Some("plan-review:e-1".into()),
                },
            ),
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
            &entry(
                "e-9",
                JournalWireEvent::PlanReview {
                    state: "resolved".into(),
                    plan_file: None,
                    title: None,
                    content: None,
                    request_id: None,
                },
            ),
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
            &entry(
                "e-1",
                JournalWireEvent::BackgroundTask {
                    snapshot: snapshot.clone(),
                },
            ),
        );
        let work: Vec<_> = emitted
            .iter()
            .filter(|e| e.channel == "x-manox-work:/s-1")
            .collect();
        assert_eq!(
            work.len(),
            1,
            "one journal row, one work-channel action: {emitted:?}"
        );
        let action = serde_json::to_value(&work[0].action).expect("action serializes");
        assert_eq!(action["type"], "x-manox-work/backgroundTasksChanged");
        assert_eq!(
            action["snapshot"], snapshot,
            "the canonical payload key is `snapshot`"
        );

        // The reference client fold: the row lands in the registry view
        // keyed by its task id.
        let mut state = crate::ext::XManoxState::default();
        assert_eq!(
            crate::ext::reducer::apply(&mut state, &action),
            crate::ext::ExtOutcome::Applied
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
        let decoded: crate::ext::XManoxState =
            serde_json::from_value(encoded).expect("state round-trips");
        assert_eq!(decoded.background_tasks, state.background_tasks);
    }
}

#[cfg(test)]
mod turn_message_tests {
    use super::*;

    #[test]
    fn the_initiating_user_row_becomes_the_turns_opening_message() {
        let mut t = Translator::new();
        let row = |seq: u64, event: JournalWireEvent| JournalWireEntry {
            seq,
            id: format!("e-{seq}"),
            parent_id: None,
            timestamp: "2026-09-29T00:00:00.000Z".into(),
            event,
        };
        let mut out = Vec::new();
        for entry in [
            row(1, JournalWireEvent::TurnStart { owner: None }),
            row(
                2,
                JournalWireEvent::Message {
                    role: "user".into(),
                    content: vec![json!({"type": "text", "text": "hello"})],
                    usage: None,
                    origin_rpc: Some("rpc-1".into()),
                    display: None,
                },
            ),
            row(3, JournalWireEvent::AgentTextDelta { s: "hi".into() }),
        ] {
            out.extend(t.on_entry("c-1", "s-1", &entry));
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
        let row = |seq: u64, event: JournalWireEvent| JournalWireEntry {
            seq,
            id: format!("e-{seq}"),
            parent_id: None,
            timestamp: "2026-09-29T00:00:00.000Z".into(),
            event,
        };
        let mut out = Vec::new();
        out.extend(t.on_entry(
            "c-1",
            "s-1",
            &row(
                1,
                JournalWireEvent::TurnStart {
                    owner: Some(manox_journal::TurnOwner { pid: 4242 }),
                },
            ),
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
