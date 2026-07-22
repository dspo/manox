//! Conversation view state.
//!
//! A gpui `Entity` holding one `Entity<MessageItem>` per conversation item.
//! `Thread` holds the canonical messages; this maintains a render-oriented
//! view: thinking and body text split into separate items, and tool calls are
//! tracked by id for status/output. Each item lives in its own `Entity` so a
//! streaming delta notifies (and re-renders) only that item, leaving already-
//! finished items' markdown untouched.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use agent::db::{UiNoteKind, UiNoteRecord};
use agent::language_model::{MessageContent, Role, StopReason};
use agent::thread::ApprovalMode;
use agent::{Message, ThreadEvent, TokenUsage, ToolCallStatus};
use gpui::{App, AppContext as _, Entity, SharedString, WeakEntity};

use crate::Workspace;
use crate::views::message::{MessageItem, build_items};

/// A decoded image attached to a user message, kept only for UI preview. The
/// canonical bytes live in the `Thread`'s `MessageContent::Image`; this holds
/// a gpui image so the user bubble can render a thumbnail without re-decoding.
#[derive(Debug, Clone)]
pub struct UserImage(pub Arc<gpui::Image>);

#[derive(Debug, Clone)]
pub struct UserTurnMeta {
    pub timestamp: i64,
    pub model_id: String,
    pub approval_mode: Option<ApprovalMode>,
    /// True when this user message entered the message list via the steer
    /// queue drain (mid-turn injection) rather than starting a fresh turn.
    /// Mirrors `MessageUiMetadata::steered`; set by the drain-driven enqueue
    /// path so `render_user` can show a "steered" badge and historical reload
    /// keeps the marker.
    pub steered: bool,
}

/// Live-only presentation state for a user bubble submitted as a steer.
/// Canonical history only contains confirmed steers, so rebuilt messages are
/// always `Normal`. A rolled-back item stays as an invisible tombstone to keep
/// the stable ids of later `MessageItem` entities intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserMessageDisplayState {
    Normal,
    PendingSteer { message_id: String },
    RolledBackSteer { message_id: String },
}

impl UserTurnMeta {
    pub fn new(timestamp: i64, model_id: String, approval_mode: Option<ApprovalMode>) -> Self {
        Self {
            timestamp,
            model_id,
            approval_mode,
            steered: false,
        }
    }

    pub(crate) fn from_message(message: &Message) -> Self {
        let ui = message.ui.as_ref();
        Self {
            timestamp: message.timestamp,
            model_id: ui.and_then(|m| m.model_id.clone()).unwrap_or_default(),
            approval_mode: ui.and_then(|m| m.approval_mode).map(ApprovalMode::from_i64),
            steered: ui.and_then(|m| m.steered).unwrap_or(false),
        }
    }
}

/// A single renderable conversation item.
#[derive(Debug, Clone)]
pub enum ConvItem {
    User {
        text: String,
        images: Vec<UserImage>,
        meta: Option<UserTurnMeta>,
        display_state: UserMessageDisplayState,
    },
    Assistant {
        text: String,
        streaming: bool,
        /// Per-turn token usage (input/output/cache) for the user message that
        /// preceded this assistant reply. Populated on turn `Stop`; `None`
        /// while streaming or when the provider didn't report usage.
        token_usage: Option<TokenUsage>,
        /// Summary of the activity segment (thinking rounds + tool calls +
        /// elapsed) that immediately preceded this reply within the same user
        /// turn — rendered inline on the model row. `None` when the reply had
        /// no preceding thinking/tool activity (a pure-text answer), so the
        /// model row stays bare. Snapshot at `close_segment_for_text` (live)
        /// / `build_items`'s `close_segment` (reload); the container keeps
        /// evolving after the snapshot but this stays frozen for the row.
        activity_summary: Option<ActivitySummary>,
    },
    /// One contiguous activity segment within a user turn: a Claude Code–style
    /// status line ("Thought for 28s, read 1 file, edited 2 files, ran 3
    /// commands") over an expandable `⎿` list of the segment's tool calls. A
    /// segment spans the whole tool-use loop of a turn — `StopReason::ToolUse`
    /// (the model paused to run a tool) does NOT close it; only a terminal stop
    /// (`EndTurn`/`MaxTokens`/`Refusal`/cancel/error) freezes the segment.
    /// Collapsed + frozen shows only the summary; collapsed + streaming also
    /// shows the running/latest entry; expanded lists every entry, each itself
    /// expandable to its full tool output.
    Thinking(ThinkingContainer),
    /// A top-level tool-call card — the `AskUserQuestion` clarify card while
    /// pending and its answered-state fallback, plus any defensive orphan.
    ToolCall(ToolCallItem),
    AgentTask(AgentTaskItem),
    /// A runtime error from the agent (red danger styling).
    Error(String),
    /// An ephemeral system notice — status changes, slash-command acks, etc.
    /// Rendered with neutral tones, not danger colors.
    Notice(String),
    /// A context-compaction summary: older history was folded into this handoff
    /// note. The summary is model-generated text (rendered as markdown, not
    /// localized); only the card title goes through i18n. Collapsible like a
    /// reasoning block — collapsed by default so the recap stays out of the
    /// way until the user wants to inspect what was dropped.
    Recap {
        summary: String,
        collapsed: bool,
        user_toggled: bool,
    },
    /// Provider is retrying the HTTP handshake after a transient failure
    /// (429 / 5xx / network). Transient: the first real content or terminal
    /// error event replaces it in place. `reason` is a short label shown on the
    /// badge; `detail` is the truncated provider body shown when expanded.
    /// `collapsed` / `user_toggled` preserve the user's expand choice across
    /// coalesced attempts.
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
        reason: String,
        detail: Option<String>,
        collapsed: bool,
        user_toggled: bool,
    },
    /// A peer message from a team member (or the leader) — `SendMessage` /
    /// broadcast delivery routed through `ThreadEvent::PeerMessage`. The `from`
    /// is a member name (data); `content` is the peer's own message body
    /// (model-generated, left verbatim). Rendered as a distinct bubble so team
    /// chatter reads apart from the user/assistant thread.
    TeamMessage {
        from: String,
        content: String,
    },
    /// A plan review item rendered as a bordered card in the message list.
    /// Carries the finalized `<proposed_plan>` text so the user can read it
    /// inline. `active` distinguishes the one plan currently awaiting a
    /// verdict (drawer + footer buttons) from prior plans already consumed by
    /// a verdict or a free-form message (plain read-only record, no buttons) —
    /// a consumed plan must not be re-judgeable.
    PlanReview {
        plan_text: String,
        active: bool,
    },
    /// A background task status card — shows a Monitor or background Bash
    /// task's kind, description, task ID, status, event count, and Stop
    /// button while running. Updated in-place by task ID.
    BackgroundTask(BackgroundTaskItem),
}

/// A background task card in the conversation, keyed by task ID so the UI
/// can update it in-place as events arrive.
#[derive(Debug, Clone)]
pub struct BackgroundTaskItem {
    pub task_id: String,
    pub kind: agent::background_task::TaskKind,
    pub description: String,
    pub status: agent::background_task::TaskStatus,
    pub event_count: u64,
    pub total_bytes: u64,
    pub exit_code: Option<i32>,
    pub failure_summary: Option<String>,
    pub created_at: Option<std::time::Instant>,
    /// Recent events (for display in an expandable body).
    pub recent_events: Vec<String>,
}

impl ConvItem {
    /// Whether this item is a `BackgroundTask` card with the given task ID.
    pub fn is_background_task_with_id(&self, task_id: &str) -> bool {
        match self {
            ConvItem::BackgroundTask(bt) => bt.task_id == task_id,
            _ => false,
        }
    }
}

/// A tool-call item, tracking status/output by id.
#[derive(Debug, Clone)]
pub struct ToolCallItem {
    pub id: String,
    pub name: String,
    pub title: String,
    pub status: ToolCallStatus,
    pub output: String,
    pub is_error: bool,
    /// The structured tool input, used for aggregate counts by target (file
    /// path / command / pattern) without re-parsing the localized title.
    /// Populated from `ThreadEvent::ToolCall` (live) or the persisted
    /// `MessageContent::ToolUse` (history rebuild). Empty for orphan
    /// `ToolResult`s with no matching `ToolCall`.
    pub input: serde_json::Value,
    /// True while live `ToolOutput` chunks are still streaming in; flipped to
    /// false once the final `ToolResult` lands the canonical output.
    pub streaming: bool,
    /// True ⇒ body hidden. Auto-flipped to true on terminal status (Success /
    /// Error / Denied) unless `user_toggled` is set, so a completed tool call
    /// collapses back to a single-line card. While `streaming` is true the
    /// body is always shown regardless of this flag.
    pub collapsed: bool,
    /// Becomes true the first time the user clicks the card header. Once
    /// set, the auto-collapse logic stops touching `collapsed` so the user's
    /// manual choice survives subsequent status transitions within the same
    /// tool call.
    pub user_toggled: bool,
    /// Persistent `Entity<TerminalPanel>` carrying the terminal-styled output
    /// body + document-level selection. `None` until first sync
    /// (`MessageItem::sync_tool_*_panel`), so a freshly constructed entry
    /// renders the per-frame fallback until the streaming/rebuild path mounts
    /// the persistent panel — mirroring the reasoning `markdown` field.
    pub panel: Option<Entity<manox_components::markdown::TerminalPanel>>,
}

/// An entry within a `ThinkingContainer`'s activity segment. A segment mixes
/// reasoning rounds (model thinking text) and tool calls into one unified tree
/// so the collapsed header can summarize the whole turn's activity.
#[derive(Debug, Clone)]
pub enum ActivityEntry {
    /// One reasoning round: a contiguous run of `AgentThinking` deltas. A new
    /// round starts when thinking resumes after being interrupted by a tool
    /// call, assistant text, or terminal stop. Auto-collapses when streaming
    /// ends unless the user manually expanded it.
    Reasoning {
        text: String,
        streaming: bool,
        collapsed: bool,
        user_toggled: bool,
        /// Persistent `Entity<Markdown>` carrying parse-once incremental parsing
        /// and document-level selection, mirroring the top-level reasoning body.
        /// `None` until first sync (`MessageItem::sync_reasoning_entry`), so a
        /// freshly constructed entry renders a per-frame fallback until the
        /// streaming/rebuild path mounts the persistent document.
        markdown: Option<Entity<manox_components::markdown::Markdown>>,
    },
    /// One tool invocation (reuses `ToolCallItem` for status/output/collapse).
    Tool(ToolCallItem),
}

/// Frozen snapshot of an activity segment's totals, rendered on the model row
/// of the assistant reply that immediately follows the segment: thinking
/// round count, tool-call count, and the segment's elapsed seconds. Built by
/// `ThinkingContainer::activity_summary` at the moment the segment closes
/// (`close_segment_for_text` live / `close_segment` on reload), so the row
/// reflects the segment as it was when the reply began — later segment
/// mutations don't move the row. `duration_secs` is `None` for segments
/// rebuilt from persisted history where the elapsed is unknown.
#[derive(Debug, Clone)]
pub struct ActivitySummary {
    pub thinking_rounds: usize,
    pub tool_calls: usize,
    pub duration_secs: Option<u64>,
}

/// One contiguous activity segment within a user turn, rendered as a Claude
/// Code–style Thinking status line. Entries are `ActivityEntry` (reasoning
/// rounds + tool calls); the container owns the segment-level summary,
/// collapse, and elapsed-time state.
///
/// A segment spans the full tool-use loop of a user turn: a `StopReason::ToolUse`
/// (the model paused to execute a tool) does NOT close it — `accepting_entries`
/// stays true so the next model response's tool calls fold into the same
/// segment. Only a terminal stop (`EndTurn`/`MaxTokens`/`Refusal`/cancel/error)
/// flips `accepting_entries` off and freezes the elapsed time.
#[derive(Debug, Clone)]
pub struct ThinkingContainer {
    /// The segment's activity entries in arrival order: reasoning rounds and
    /// tool calls interleaved as they occurred during the turn.
    pub entries: Vec<ActivityEntry>,
    /// True while the turn is still in progress — new `ToolCall` events fold
    /// into this segment rather than opening a fresh one. `StopReason::ToolUse`
    /// keeps it true; a terminal `Stop` flips it off. Independent of whether
    /// any individual entry is currently live.
    pub accepting_entries: bool,
    /// True while the segment is live (turn running) OR any entry is still
    /// streaming / non-terminal. Drives the spinner + "Thinking for Xs" label
    /// vs the frozen "Thought for Xs".
    pub streaming: bool,
    /// True ⇒ collapsed shows only the summary line (frozen) or the summary
    /// plus the running/latest entry (streaming). False ⇒ every entry renders.
    /// Auto-flipped to true on terminal `Stop` unless the user toggled.
    pub collapsed: bool,
    pub user_toggled: bool,
    /// When the segment started; the live "for Xs" is
    /// `started_at.elapsed().as_secs()`, recomputed each render while the
    /// ticker fires. Seeded from the turn's start time so the duration covers
    /// the whole turn, not just from the first `ToolCall`.
    pub started_at: Instant,
    /// The elapsed seconds captured when the segment went terminal. Once set,
    /// "Thought for Xs" renders this fixed value instead of re-reading
    /// `started_at` (which would keep growing on every later re-render). `None`
    /// for freshly rebuilt historical segments, where the duration is unknown
    /// and the label degrades to a bare "Thought".
    pub frozen_secs: Option<u64>,
}

impl ThinkingContainer {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            accepting_entries: true,
            streaming: true,
            collapsed: false,
            user_toggled: false,
            started_at: Instant::now(),
            frozen_secs: None,
        }
    }

    /// Re-derive `streaming` from `accepting_entries` and the entries' live
    /// flags + statuses. Call after any entry mutation that may have flipped a
    /// status or streaming flag. The segment stays live (`streaming == true`)
    /// as long as it is still accepting entries — even if every entry is
    /// terminal — because a `StopReason::ToolUse` means the turn continues and
    /// more tool calls will arrive. Once `accepting_entries` is false (terminal
    /// stop) and all entries are terminal, `streaming` goes false and the
    /// elapsed is pinned here at the real turn-completion moment. Idempotent:
    /// a later call re-derives the same `false` but leaves an already-pinned
    /// value untouched.
    pub fn recompute_streaming(&mut self) {
        let was_streaming = self.streaming;
        let any_entry_live = self.entries.iter().any(|e| match e {
            ActivityEntry::Reasoning { streaming, .. } => *streaming,
            ActivityEntry::Tool(t) => {
                t.streaming
                    || matches!(
                        t.status,
                        ToolCallStatus::Running | ToolCallStatus::PendingApproval
                    )
            }
        });
        self.streaming = self.accepting_entries || any_entry_live;
        if was_streaming && !self.streaming && self.frozen_secs.is_none() {
            self.frozen_secs = Some(self.started_at.elapsed().as_secs());
        }
    }

    /// Freeze the segment for a terminal stop (`EndTurn`/`MaxTokens`/`Refusal`/
    /// cancel/error). Stops accepting entries, flips `streaming` off, and pins
    /// the elapsed time. Idempotent.
    pub fn finalize_segment(&mut self) {
        self.accepting_entries = false;
        if self.frozen_secs.is_none() {
            self.frozen_secs = Some(self.started_at.elapsed().as_secs());
        }
        self.streaming = false;
    }
    /// Flip every streaming reasoning round's `streaming` flag off and re-derive
    /// `streaming`, WITHOUT closing the segment — `accepting_entries` stays true
    /// so subsequent tool calls still fold in. Called on a mid-turn
    /// `Stop(ToolUse)`: the current reasoning round is done (the model emitted a
    /// tool call), so the next `AgentThinking` must open a fresh round instead
    /// of appending to this one (which would show the prior round's body still
    /// growing while the answer text is already rendering). Mirrors the
    /// reasoning-finalization half of `close_for_text` without the
    /// `accepting_entries = false` / elapsed-pin that only a text boundary or
    /// terminal stop warrants. Idempotent.
    pub fn finalize_reasoning_rounds(&mut self) {
        for entry in &mut self.entries {
            if let ActivityEntry::Reasoning { streaming, .. } = entry {
                *streaming = false;
            }
        }
        self.recompute_streaming();
    }

    /// Stop accepting entries because assistant text has arrived mid-turn —
    /// the model moved on to the answer, so this segment's reasoning/tool
    /// activity is done. Mirrors `build_items`'s `close_segment` on
    /// `MessageContent::Text`: sets `accepting_entries = false` so subsequent
    /// `AgentThinking` opens a fresh segment instead of folding into this one
    /// (temporal inversion, issue #216). Also finalizes any still-streaming
    /// reasoning rounds (idempotent with `finalize_reasoning_rounds`, which a
    /// mid-turn `Stop(ToolUse)` already ran) and re-derives `streaming` so the
    /// spinner stops and the elapsed timer pins. Unlike `finalize_segment`
    /// (terminal stop), does NOT auto-collapse entries or the container: the
    /// user may be inspecting the activity tree.
    pub fn close_for_text(&mut self) {
        self.finalize_reasoning_rounds();
        self.accepting_entries = false;
        self.recompute_streaming();
    }

    /// True when at least one entry has produced output worth showing (the
    /// `⎿` list is non-vacuous even while collapsed — the running/latest
    /// entry's summary is the "what's happening right now" line).
    pub fn has_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Find a tool entry by id. Returns its index within `entries`.
    pub fn find_tool_entry_index(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| match e {
            ActivityEntry::Tool(t) => t.id == id,
            _ => false,
        })
    }

    /// Get a mutable reference to a tool entry by id.
    pub fn get_tool_entry_mut(&mut self, id: &str) -> Option<&mut ToolCallItem> {
        self.entries.iter_mut().find_map(|e| match e {
            ActivityEntry::Tool(t) if t.id == id => Some(t),
            _ => None,
        })
    }

    /// Get a reference to the last reasoning entry if it is still streaming.
    /// Used by `apply()` to decide whether to append deltas to the existing
    /// reasoning round or start a new one.
    pub fn last_streaming_reasoning_index(&self) -> Option<usize> {
        self.entries.iter().rposition(|e| {
            matches!(
                e,
                ActivityEntry::Reasoning {
                    streaming: true,
                    ..
                }
            )
        })
    }

    /// Build a frozen snapshot of the segment's totals for the model row of
    /// the assistant reply that immediately follows it. Returns `None` when
    /// the segment had no reasoning rounds and no tool calls — the reply
    /// renders a bare model row with no activity suffix.
    pub fn activity_summary(&self) -> Option<ActivitySummary> {
        let thinking_rounds = self
            .entries
            .iter()
            .filter(|e| matches!(e, ActivityEntry::Reasoning { .. }))
            .count();
        let tool_calls = self
            .entries
            .iter()
            .filter(|e| matches!(e, ActivityEntry::Tool(_)))
            .count();
        if thinking_rounds == 0 && tool_calls == 0 {
            return None;
        }
        Some(ActivitySummary {
            thinking_rounds,
            tool_calls,
            duration_secs: self.frozen_secs,
        })
    }
}

impl Default for ThinkingContainer {
    fn default() -> Self {
        Self::new()
    }
}

/// A sub-agent (`agent` tool) invocation. Displayed as a single-line clickable
/// item in the parent message flow; the full child conversation is observed in
/// a read-only `SubagentPanel` tab owned by the workspace.
#[derive(Debug, Clone)]
pub struct AgentTaskItem {
    pub id: String,
    pub subagent_type: String,
    pub description: String,
    pub status: ToolCallStatus,
    pub is_error: bool,
}

pub(crate) fn agent_task_labels(input: &serde_json::Value) -> (String, String) {
    let subagent_type = input
        .get("subagent_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let description = input
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    (subagent_type, description)
}

#[derive(Debug)]
pub struct ConversationState {
    items: Vec<Entity<MessageItem>>,
    /// The start instant of the current (or most recent) user turn, captured on
    /// `ThreadEvent::TurnStarted`. Seeded into each new activity segment's
    /// `started_at` so the elapsed covers the whole turn — reasoning warmup,
    /// model latency, and every tool-use loop iteration — not just from the
    /// first `ToolCall`. Falls back to `Instant::now()` for the rebuild path
    /// (where durations are unknown anyway).
    turn_started_at: Instant,
}

impl Default for ConversationState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            turn_started_at: Instant::now(),
        }
    }
}

/// Workspace context threaded through `apply` / `rebuild_from_messages`: the
/// weak handle (for item toggle callbacks) plus the thread cwd snapshot (for
/// the `TerminalPanel` prompt line). Bundled so the signatures stay under
/// clippy's argument-count limit. The cwd is a per-call snapshot taken by the
/// caller from the `Thread` entity — reading the `Workspace` itself would
/// double-lease inside a `Workspace::update`.
pub struct ApplyCtx {
    pub weak: WeakEntity<Workspace>,
    pub cwd: Option<SharedString>,
}

impl ConversationState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[Entity<MessageItem>] {
        &self.items
    }

    /// True when the conversation has no substantive items (user, assistant,
    /// reasoning, tool call, or agent task). Notice-only items (error cards
    /// used for slash-command acknowledgements and mode switches) don't count
    /// so toggling YOLO on the empty first screen doesn't prematurely leave
    /// the hero layout.
    pub fn is_empty(&self, cx: &App) -> bool {
        self.items.iter().all(|e| {
            matches!(
                e.read(cx).kind(),
                ConvItem::Error(_)
                    | ConvItem::Notice(_)
                    | ConvItem::User {
                        display_state: UserMessageDisplayState::RolledBackSteer { .. },
                        ..
                    }
            )
        })
    }

    /// Append a user message with any pasted/image attachments.
    pub fn push_user(
        &mut self,
        text: String,
        images: Vec<UserImage>,
        meta: UserTurnMeta,
        weak: WeakEntity<Workspace>,
        cx: &mut App,
    ) {
        let id = self.items.len();
        let role = meta.model_id.clone();
        self.items.push(cx.new(|_| {
            MessageItem::new(
                ConvItem::User {
                    text,
                    images,
                    meta: Some(meta),
                    display_state: UserMessageDisplayState::Normal,
                },
                role,
                id,
                weak,
            )
        }));
    }

    /// Append a steer bubble immediately after the user clicks Steer. The
    /// canonical `Thread::messages` entry is still parked in `pending_steer`;
    /// `confirm_pending_steer` turns this optimistic bubble into normal history
    /// only after `ThreadEvent::SteerInjected` acknowledges the drain.
    pub fn push_pending_steer(
        &mut self,
        text: String,
        images: Vec<UserImage>,
        meta: UserTurnMeta,
        message_id: String,
        weak: WeakEntity<Workspace>,
        cx: &mut App,
    ) {
        let id = self.items.len();
        let role = meta.model_id.clone();
        self.items.push(cx.new(|_| {
            MessageItem::new(
                ConvItem::User {
                    text,
                    images,
                    meta: Some(meta),
                    display_state: UserMessageDisplayState::PendingSteer { message_id },
                },
                role,
                id,
                weak,
            )
        }));
    }

    /// Confirm an optimistic steer. This also heals a provisional rollback: a
    /// terminal Stop can arrive before the run loop performs its final drain.
    pub fn confirm_pending_steer(&mut self, message_id: &str, cx: &mut App) -> bool {
        for entity in &self.items {
            let matches = matches!(
                entity.read(cx).kind(),
                ConvItem::User {
                    display_state: UserMessageDisplayState::PendingSteer { message_id: id }
                        | UserMessageDisplayState::RolledBackSteer { message_id: id },
                    ..
                } if id == message_id
            );
            if matches {
                entity.update(cx, |item, cx| {
                    if let ConvItem::User {
                        meta,
                        display_state,
                        ..
                    } = item.kind_mut()
                    {
                        if let Some(meta) = meta {
                            meta.steered = true;
                        }
                        *display_state = UserMessageDisplayState::Normal;
                    }
                    cx.notify();
                });
                return true;
            }
        }
        false
    }

    /// Hide an optimistic steer that the running turn never absorbed. The
    /// owning queue item is restored separately by `Workspace`.
    pub fn rollback_pending_steer(&mut self, message_id: &str, cx: &mut App) -> bool {
        for entity in &self.items {
            let matches = matches!(
                entity.read(cx).kind(),
                ConvItem::User {
                    display_state: UserMessageDisplayState::PendingSteer { message_id: id },
                    ..
                } if id == message_id
            );
            if matches {
                entity.update(cx, |item, cx| {
                    if let ConvItem::User {
                        meta,
                        display_state,
                        ..
                    } = item.kind_mut()
                    {
                        if let Some(meta) = meta {
                            meta.steered = false;
                        }
                        *display_state = UserMessageDisplayState::RolledBackSteer {
                            message_id: message_id.to_string(),
                        };
                    }
                    cx.notify();
                });
                return true;
            }
        }
        false
    }

    /// Append a system-styled notice. Does not touch the canonical `Thread`
    /// messages — UI-only, for slash-command acknowledgements and similar
    /// ephemeral notices.
    pub fn push_notice(&mut self, text: String, weak: WeakEntity<Workspace>, cx: &mut App) {
        let id = self.items.len();
        self.items
            .push(cx.new(|_| MessageItem::new(ConvItem::Notice(text), String::new(), id, weak)));
    }

    /// Append a plan-review item to the message list. The plan text renders
    /// inline as a read-only bordered card with a height-limited markdown body.
    /// Pushed `active` — a fresh `PlanReady` always awaits a verdict; the card
    /// is demoted to an inactive record by `consume_plan_review` once the user
    /// acts on it (verdict or free-form message).
    pub fn push_plan_review(
        &mut self,
        plan_text: String,
        role: String,
        weak: WeakEntity<Workspace>,
        cx: &mut App,
    ) {
        let id = self.items.len();
        self.items.push(cx.new(|_| {
            MessageItem::new(
                ConvItem::PlanReview {
                    plan_text,
                    active: true,
                },
                role,
                id,
                weak,
            )
        }));
    }

    /// Mark the most recent plan-review card as no longer actionable: a verdict
    /// was clicked or a free-form message superseded it. Only the tail plan can
    /// be active (every prior one was already consumed when its turn ended), so
    /// the first `PlanReview` found scanning from the tail is the one to demote.
    pub fn consume_plan_review(&mut self, cx: &mut App) {
        for item in self.items.iter().rev() {
            let is_active_plan = matches!(
                item.read(cx).kind(),
                ConvItem::PlanReview { active: true, .. }
            );
            if is_active_plan {
                item.update(cx, |it, cx| {
                    if let ConvItem::PlanReview { active, .. } = it.kind_mut() {
                        *active = false;
                    }
                    cx.notify();
                });
                break;
            }
        }
    }

    /// Drop the most recent plan-review card outright. Called only on an
    /// implement verdict, where the pending plan card is by construction the
    /// live tail (a fresh `PlanReady` always lands at the tail and nothing is
    /// pushed between the card and the verdict) — so a tail pop is the safe
    /// removal. The verdict's own user bubble is pushed in its place, carrying
    /// the same plan text the thread injects, so live and rebuilt views match.
    pub fn pop_plan_review_tail(&mut self, cx: &mut App) -> bool {
        let is_plan = self
            .items
            .last()
            .map(|last| matches!(last.read(cx).kind(), ConvItem::PlanReview { .. }))
            .unwrap_or(false);
        if is_plan {
            self.items.pop();
        }
        is_plan
    }

    pub fn find_tool(&self, id: &str, cx: &App) -> Option<usize> {
        self.items
            .iter()
            .position(|e| matches!(e.read(cx).kind(), ConvItem::ToolCall(t) if t.id == id))
    }

    fn find_agent_task(&self, id: &str, cx: &App) -> Option<usize> {
        self.items
            .iter()
            .position(|e| matches!(e.read(cx).kind(), ConvItem::AgentTask(t) if t.id == id))
    }

    /// Locate a `Thinking` container's tool entry by id. Returns
    /// `(container_index, entry_index)` so the caller can update the entry in
    /// place. Scans every container in arrival order so an id always resolves
    /// to its owning batch regardless of which trailing container is active.
    pub fn find_thinking_entry(&self, id: &str, cx: &App) -> Option<(usize, usize)> {
        for (cix, e) in self.items.iter().enumerate() {
            if let ConvItem::Thinking(t) = e.read(cx).kind()
                && let Some(eix) = t.find_tool_entry_index(id)
            {
                return Some((cix, eix));
            }
        }
        None
    }

    /// The index of the active activity segment — a `Thinking` container that
    /// is still accepting entries (the turn is in progress, surviving
    /// `StopReason::ToolUse`). A new `ToolCall` folds into this segment; `None`
    /// when no live segment exists, in which case the caller opens a fresh one.
    /// Scans from the tail so the most recent live segment wins.
    fn find_active_activity_segment(&self, cx: &App) -> Option<usize> {
        self.items.iter().rposition(
            |e| matches!(e.read(cx).kind(), ConvItem::Thinking(t) if t.accepting_entries),
        )
    }

    /// Apply a `ThreadEvent` delta (excludes `ToolCallAuthorization`, which `Workspace` handles).
    /// `last_request_usage` is the token usage for the turn's last user message;
    /// consumed only on `Stop` to label the just-finished assistant reply.
    pub fn apply(
        &mut self,
        event: &ThreadEvent,
        role: &str,
        last_request_usage: Option<TokenUsage>,
        ctx: ApplyCtx,
        cx: &mut App,
    ) {
        let ApplyCtx { weak, cwd } = ctx;
        // A trailing `Retry` badge is stale the moment a real content or
        // terminal-error event lands — that event means the retry either
        // succeeded (assistant text / tool call) or exhausted the budget
        // (Error). Pop the badge first so the arm below pushes its own item
        // into the freed slot. Non-item events (usage, mode change, …) and
        // the `Retry` event itself skip this.
        if matches!(
            event,
            ThreadEvent::AgentText(_)
                | ThreadEvent::AgentThinking(_)
                | ThreadEvent::ToolCall { .. }
                | ThreadEvent::Error(_)
                | ThreadEvent::Compaction { .. }
        ) {
            self.pop_trailing_retry(cx);
        }

        match event {
            // A compaction landed — render the handoff summary as a Recap card.
            // The card is appended (never updated in place): a compaction is a
            // one-time boundary marker, and the summary text is final.
            ThreadEvent::Compaction { summary, .. } => {
                let id = self.items.len();
                self.items.push(cx.new(|_| {
                    MessageItem::new(
                        ConvItem::Recap {
                            summary: summary.clone(),
                            collapsed: true,
                            user_toggled: false,
                        },
                        role.to_string(),
                        id,
                        weak,
                    )
                }));
            }
            // `<proposed_plan>` streaming + completion are owned by the
            // workspace's review overlay, not the conversation list — there is
            // no ToolCall card to backfill (the plan arrives as a text block,
            // not a tool call).
            // `PlanUpdated` mirrors to the Context Rail (handled in the
            // workspace subscription), not the conversation list.
            ThreadEvent::PlanDelta { .. }
            | ThreadEvent::PlanReady { .. }
            | ThreadEvent::PlanUpdated { .. } => {}
            // Token usage + model/effort changes are surfaced elsewhere (sidebar /
            // model-history overlay). No conversation item.
            ThreadEvent::TokenUsageUpdated(_)
            | ThreadEvent::ModelChanged { .. }
            | ThreadEvent::ReasoningEffortChanged { .. }
            // `CompactionStarted` is a cockpit-only phase signal (the side-LLM
            // summarization is in flight); the conversation list renders nothing
            // for it. The workspace flips the cockpit phase on this event.
            | ThreadEvent::CompactionStarted { .. } => {},
            // `SteerInjected` is fired by the turn loop at drain time. The
            // workspace owns the queue→list transition (it pairs the event with
            // the matching `SteerPending` queue card and pushes the bubble here
            // via `push_user`); the conversation list takes no direct action.
            | ThreadEvent::SteerInjected { .. } => {},
            // `TurnStarted` is a UI-only signal routed to `ThreadStore` by the
            // workspace to light the sidebar running indicator; it carries no
            // conversation content. We capture the turn's start instant here so
            // the first activity segment's elapsed covers the whole turn
            // (reasoning warmup + model latency), not just from the first
            // `ToolCall`.
            ThreadEvent::TurnStarted => {
                self.turn_started_at = Instant::now();
            }
            ThreadEvent::TurnFinished { .. } => {}
            // Goal lifecycle is surfaced by the composer chip + status popover,
            // not as a conversation item.
            ThreadEvent::GoalChanged { .. } | ThreadEvent::GoalEvaluated { .. } => {}
            ThreadEvent::AgentText(delta) => {
                let needs_new = match self.items.last() {
                    Some(e) => !matches!(
                        e.read(cx).kind(),
                        ConvItem::Assistant {
                            streaming: true,
                            ..
                        }
                    ),
                    None => true,
                };
                if needs_new {
                    // Close the active activity segment so subsequent
                    // `AgentThinking` opens a fresh one — mirrors
                    // `build_items`'s `close_segment` on `MessageContent::Text`.
                    // Without this, thinking arriving after the answer text
                    // folds into the pre-answer segment (issue #216).
                    // Snapshot the just-closed segment's totals for the model
                    // row. Read *after* `close_segment_for_text` so
                    // `frozen_secs` is pinned — the row shows the segment's
                    // elapsed instead of a live timer that would keep growing
                    // on every re-render of the reply.
                    let activity_summary = if let Some(cix) = self.find_active_activity_segment(cx) {
                        self.items[cix].update(cx, |item, cx| {
                            item.close_segment_for_text(cx);
                            cx.notify();
                        });
                        match self.items[cix].read(cx).kind() {
                            ConvItem::Thinking(t) => t.activity_summary(),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    let id = self.items.len();
                    self.items.push(cx.new(|cx| {
                        let mut item = MessageItem::new(
                            ConvItem::Assistant {
                                text: delta.clone(),
                                streaming: true,
                                token_usage: None,
                                activity_summary,
                            },
                            role.to_string(),
                            id,
                            weak,
                        );
                        item.update_text(delta, cx);
                        item
                    }));
                } else {
                    let ix = self.items.len() - 1;
                    self.items[ix].update(cx, |item, cx| {
                        // Snapshot the text *after* appending the delta so the
                        // parser and the `text` field (the copy-button source)
                        // stay in lockstep — feeding a pre-append snapshot here
                        // would render the body one delta behind forever, and
                        // `finalize()` on stream stop would re-parse that stale
                        // text, permanently dropping the last delta.
                        let full_text = match item.kind_mut() {
                            ConvItem::Assistant { text, .. } => {
                                text.push_str(delta);
                                text.clone()
                            }
                            _ => return,
                        };
                        item.update_text(&full_text, cx);
                        cx.notify();
                    });
                }
            }
            ThreadEvent::AgentThinking(delta) => {
                // Fold reasoning into the active activity segment. A contiguous
                // run of deltas appends to the last streaming reasoning entry;
                // a gap (interrupted by a tool call or new turn) starts a
                // fresh round. If no segment exists yet, open one.
                let turn_started_at = self.turn_started_at;
                let cix = match self.find_active_activity_segment(cx) {
                    Some(i) => i,
                    None => {
                        let i = self.items.len();
                        self.items.push(cx.new(|_| {
                            let mut container = ThinkingContainer::new();
                            container.started_at = turn_started_at;
                            MessageItem::new(
                                ConvItem::Thinking(container),
                                role.to_string(),
                                i,
                                weak.clone(),
                            )
                        }));
                        i
                    }
                };
                let delta = delta.clone();
                self.items[cix].update(cx, |item, cx| {
                    let eix = if let ConvItem::Thinking(t) = item.kind_mut() {
                        let eix = if let Some(eix) = t.last_streaming_reasoning_index() {
                            // Append to the existing streaming reasoning round.
                            if let ActivityEntry::Reasoning { text, .. } =
                                &mut t.entries[eix]
                            {
                                text.push_str(&delta);
                            }
                            eix
                        } else {
                            // Start a new reasoning round.
                            let eix = t.entries.len();
                            t.entries.push(ActivityEntry::Reasoning {
                                text: delta,
                                streaming: true,
                                collapsed: true,
                                user_toggled: false,
                                markdown: None,
                            });
                            eix
                        };
                        t.recompute_streaming();
                        Some(eix)
                    } else {
                        None
                    };
                    // Mount/sync the persistent `Entity<Markdown>` so streaming
                    // deltas drive incremental parsing + document-level
                    // selection (drag + Cmd/Ctrl+C), mirroring the top-level body.
                    if let Some(eix) = eix {
                        item.sync_reasoning_entry(eix, cx);
                    }
                    cx.notify();
                });
            }
            ThreadEvent::ToolCall {
                id,
                name,
                title,
                status,
                input,
            } => {
                if name == agent::tools::AGENT {
                    let (subagent_type, description) = input
                        .as_ref()
                        .map(agent_task_labels)
                        .unwrap_or_default();
                    if let Some(ix) = self.find_agent_task(id, cx) {
                        self.items[ix].update(cx, |item, cx| {
                            if let ConvItem::AgentTask(t) = item.kind_mut() {
                                if !subagent_type.is_empty() {
                                    t.subagent_type = subagent_type.clone();
                                }
                                if !description.is_empty() {
                                    t.description = description.clone();
                                }
                                t.status = *status;
                            }
                            cx.notify();
                        });
                    } else {
                        let ix = self.items.len();
                        self.items.push(cx.new(|_| {
                            MessageItem::new(
                                ConvItem::AgentTask(AgentTaskItem {
                                    id: id.clone(),
                                    subagent_type,
                                    description,
                                    status: *status,
                                    is_error: false,
                                }),
                                role.to_string(),
                                ix,
                                weak,
                            )
                        }));
                    }
                } else if name == agent::tools::ASK_USER_QUESTION {
                    // Top-level card, never folded into an activity segment.
                    // `AskUserQuestion` drives an inline clarify card via
                    // `render_ask_user_card` while pending and a plain answered
                    // card once its result lands.
                    if let Some(ix) = self.find_tool(id, cx) {
                        self.items[ix].update(cx, |item, cx| {
                            if let ConvItem::ToolCall(t) = item.kind_mut() {
                                t.title = title.clone();
                                t.status = *status;
                                t.name = name.clone();
                            }
                            cx.notify();
                        });
                    } else {
                        let ix = self.items.len();
                        self.items.push(cx.new(|_| {
                            MessageItem::new(
                                ConvItem::ToolCall(ToolCallItem {
                                    id: id.clone(),
                                    name: name.clone(),
                                    title: title.clone(),
                                    status: *status,
                                    output: String::new(),
                                    is_error: false,
                                    input: input.clone().unwrap_or(serde_json::Value::Null),
                                    streaming: matches!(*status, ToolCallStatus::Running),
                                    collapsed: false,
                                    user_toggled: false,
                                    panel: None,
                                }),
                                role.to_string(),
                                ix,
                                weak,
                            )
                        }));
                    }
                } else {
                    // Ordinary tool call: fold into the active activity
                    // segment. A fresh segment opens when the previous one
                    // went terminal (turn ended) or no segment exists yet —
                    // so parallel tool calls in one model response AND tool
                    // calls across the whole turn's tool-use loop aggregate
                    // into one status line. The segment is seeded with the
                    // turn's start time so the elapsed covers the whole turn.
                    let turn_started_at = self.turn_started_at;
                    let cix = match self.find_active_activity_segment(cx) {
                        Some(i) => i,
                        None => {
                            let i = self.items.len();
                            self.items.push(cx.new(|_| {
                                let mut container = ThinkingContainer::new();
                                container.started_at = turn_started_at;
                                MessageItem::new(
                                    ConvItem::Thinking(container),
                                    role.to_string(),
                                    i,
                                    weak,
                                )
                            }));
                            i
                        }
                    };
                    let id = id.clone();
                    let name = name.clone();
                    let title = title.clone();
                    let status = *status;
                    let entry_input = input.clone().unwrap_or(serde_json::Value::Null);
                    self.items[cix].update(cx, |item, cx| {
                        if let ConvItem::Thinking(t) = item.kind_mut() {
                            if let Some(entry) = t.get_tool_entry_mut(&id) {
                                entry.title = title;
                                entry.name = name;
                                entry.status = status;
                                entry.input = entry_input;
                                if matches!(
                                    status,
                                    ToolCallStatus::Success
                                        | ToolCallStatus::Error
                                        | ToolCallStatus::Denied
                                ) && !entry.streaming
                                {
                                    entry.collapsed = !entry.user_toggled;
                                }
                            } else {
                                t.entries.push(ActivityEntry::Tool(ToolCallItem {
                                    id,
                                    name,
                                    title,
                                    status,
                                    output: String::new(),
                                    is_error: false,
                                    input: entry_input,
                                    streaming: matches!(status, ToolCallStatus::Running),
                                    collapsed: true,
                                    user_toggled: false,
                                    panel: None,
                                }));
                            }
                            t.recompute_streaming();
                        }
                        cx.notify();
                    });
                }
            }
            ThreadEvent::ToolOutput { id, chunk } => {
                // Subagent text/thinking is no longer forwarded to the parent
                // message flow — the observation panel subscribes to the child
                // thread directly. Only match ThinkingContainer entries here.
                if let Some((cix, eix)) = self.find_thinking_entry(id, cx) {
                    self.items[cix].update(cx, |item, cx| {
                        if let ConvItem::Thinking(t) = item.kind_mut() {
                            if let Some(ActivityEntry::Tool(entry)) = t.entries.get_mut(eix) {
                                entry.output.push_str(chunk);
                                entry.streaming = true;
                            }
                            t.streaming = true;
                        }
                        item.sync_tool_entry_panel(eix, cwd, cx);
                        cx.notify();
                    });
                }
            }
            ThreadEvent::ToolResult {
                id,
                output,
                is_error,
            } => {
                let status = if *is_error {
                    ToolCallStatus::Error
                } else {
                    ToolCallStatus::Success
                };
                if let Some(ix) = self.find_agent_task(id, cx) {
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::AgentTask(t) = item.kind_mut() {
                            let next_status = if !*is_error && t.status == ToolCallStatus::Continued
                            {
                                ToolCallStatus::Continued
                            } else {
                                status
                            };
                            t.is_error = *is_error;
                            t.status = next_status;
                        }
                        cx.notify();
                    });
                } else if let Some((cix, eix)) = self.find_thinking_entry(id, cx) {
                    let entry_output = output.clone();
                    let entry_is_error = *is_error;
                    self.items[cix].update(cx, |item, cx| {
                        if let ConvItem::Thinking(t) = item.kind_mut() {
                            if let Some(ActivityEntry::Tool(entry)) = t.entries.get_mut(eix) {
                                entry.output = entry_output;
                                entry.is_error = entry_is_error;
                                entry.streaming = false;
                                entry.status = status;
                                entry.collapsed = !entry.user_toggled;
                            }
                            // A finalized entry does NOT close the segment —
                            // `accepting_entries` stays true across the
                            // tool-use loop. Only a terminal `Stop` freezes
                            // the segment (see the `Stop` arm). The container
                            // collapses only when the whole turn goes terminal.
                            t.recompute_streaming();
                        }
                        item.sync_tool_entry_panel(eix, cwd, cx);
                        cx.notify();
                    });
                } else if let Some(ix) = self.find_tool(id, cx) {
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::ToolCall(t) = item.kind_mut() {
                            t.output = output.clone();
                            t.is_error = *is_error;
                            t.streaming = false;
                            t.status = status;
                            // Auto-collapse once the tool call reaches a terminal
                            // status. Preserves the user's manual choice if any.
                            t.collapsed = !t.user_toggled;
                        }
                        item.sync_tool_call_panel(cwd, cx);
                        cx.notify();
                    });
                } else {
                    // No matching entry; insert as a finalized single-entry
                    // activity segment so the orphan result still renders as a
                    // `⎿` line rather than a bare ToolCall card.
                    let ix = self.items.len();
                    let entry = ToolCallItem {
                        id: id.clone(),
                        name: String::new(),
                        title: String::new(),
                        status,
                        output: output.clone(),
                        is_error: *is_error,
                        input: serde_json::Value::Null,
                        streaming: false,
                        collapsed: !matches!(
                            status,
                            ToolCallStatus::Running | ToolCallStatus::PendingApproval
                        ),
                        user_toggled: false,
                        panel: None,
                    };
                    let mut container = ThinkingContainer::new();
                    container.accepting_entries = false;
                    container.streaming = false;
                    container.collapsed = false;
                    container.entries.push(ActivityEntry::Tool(entry));
                    self.items.push(cx.new(|_| {
                        MessageItem::new(ConvItem::Thinking(container), role.to_string(), ix, weak.clone())
                    }));
                    // Mount the orphan entry's persistent panel after push — the
                    // panel needs an `&mut Context<MessageItem>` to create the
                    // Entity, which the `cx.new(|_| …)` closure above lacks.
                    self.items[ix].update(cx, |item, cx| {
                        item.sync_tool_entry_panel(0, cwd, cx);
                        cx.notify();
                    });
                }
            }
            ThreadEvent::ToolCallAuthorization { .. } => {
                // Handled by `Workspace` as a prompt overlay; not part of the conversation flow.
            }
            ThreadEvent::Stop(reason) => {
                // `StopReason::ToolUse` is mid-turn: the model paused to
                // execute a tool. Only finalize assistant/reasoning text
                // streaming; the activity segment stays open so the next
                // model response's tool calls fold into the same segment.
                // A terminal stop (`EndTurn`/`MaxTokens`/`Refusal`) freezes
                // the segment and auto-collapses everything.
                let terminal = !matches!(reason, StopReason::ToolUse);
                for e in &self.items {
                    e.update(cx, |item, cx| {
                        item.finalize_streaming(terminal, cx);
                        cx.notify();
                    });
                }
                // Stamp the per-turn usage onto the last assistant reply so its
                // footer can show input/output/cache totals for this turn. Walk
                // backward: the last item may be a tool call or reasoning block
                // emitted after the assistant text, not the assistant itself.
                if let Some(usage) = last_request_usage {
                    for e in self.items.iter().rev() {
                        let stamped = e.update(cx, |item, _cx| {
                            if let ConvItem::Assistant { token_usage, .. } = item.kind_mut() {
                                *token_usage = Some(usage);
                                true
                            } else {
                                false
                            }
                        });
                        if stamped {
                            e.update(cx, |_, cx| cx.notify());
                            break;
                        }
                    }
                }
            }
            ThreadEvent::Error(e) => {
                let ix = self.items.len();
                self.items.push(cx.new(|_| {
                    MessageItem::new(ConvItem::Error(e.to_string()), role.to_string(), ix, weak)
                }));
            }
            ThreadEvent::PeerMessage { from, content } => {
                let ix = self.items.len();
                self.items.push(cx.new(|_| {
                    MessageItem::new(
                        ConvItem::TeamMessage {
                            from: from.clone(),
                            content: content.clone(),
                        },
                        role.to_string(),
                        ix,
                        weak,
                    )
                }));
            }
            ThreadEvent::ApprovalModeChanged { .. } => {
                // UI state (badge/chip) handled by `Workspace`; not a conversation item.
            }
            ThreadEvent::PrefixStability { .. } => {
                // Cache discipline signal: no conversation item, the drift
                // flags are only consumed by debug telemetry views (if at all).
            }
            ThreadEvent::Retry {
                attempt,
                max_attempts,
                delay_secs,
                reason,
                detail,
            } => {
                // Coalesce consecutive retries into the same tail item so the
                // badge counts up in place rather than stacking a row per
                // attempt. The first retry after real content pushes a new item.
                if let Some(last) = self.items.last() {
                    let is_retry = matches!(last.read(cx).kind(), ConvItem::Retry { .. });
                    if is_retry {
                        let last = last.clone();
                        let attempt = *attempt;
                        let max_attempts = *max_attempts;
                        let delay_secs = *delay_secs;
                        let reason = reason.clone();
                        let detail = detail.clone();
                        last.update(cx, |item, cx| {
                            if let ConvItem::Retry {
                                attempt: a,
                                max_attempts: m,
                                delay_secs: d,
                                reason: r,
                                detail: det,
                                ..
                            } = item.kind_mut()
                            {
                                *a = attempt;
                                *m = max_attempts;
                                *d = delay_secs;
                                *r = reason;
                                *det = detail;
                            }
                            cx.notify();
                        });
                        return;
                    }
                }
                let id = self.items.len();
                self.items.push(cx.new(|_| {
                    MessageItem::new(
                        ConvItem::Retry {
                            attempt: *attempt,
                            max_attempts: *max_attempts,
                            delay_secs: *delay_secs,
                            reason: reason.clone(),
                            detail: detail.clone(),
                            collapsed: true,
                            user_toggled: false,
                        },
                        String::new(),
                        id,
                        weak.clone(),
                    )
                }));
            }
            ThreadEvent::SubagentProgress {
                id,
                status,
                ..
            } => {
                if let Some(ix) = self.find_agent_task(id, cx) {
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::AgentTask(t) = item.kind_mut() {
                            t.status = *status;
                        }
                        cx.notify();
                    });
                }
            }
            ThreadEvent::SubagentStarted {
                id,
                subagent_type,
                description,
                ..
            } => {
                if let Some(ix) = self.find_agent_task(id, cx) {
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::AgentTask(t) = item.kind_mut() {
                            t.subagent_type = subagent_type.clone();
                            t.description = description.clone();
                            t.status = ToolCallStatus::Running;
                        }
                        cx.notify();
                    });
                }
            }
            ThreadEvent::BrowserNotification { .. } | ThreadEvent::InboundAuthorization { .. } => {
                // Browser-axis signals are routed for the UI chrome (overlay,
                // hint, tab state), not rendered as conversation items. The
                // owning Workspace subscriber handles the surface.
            }
            ThreadEvent::BackgroundTaskUpdated { snapshot } => {
                // Find an existing BackgroundTask card with this task_id and
                // update it in-place; otherwise push a new card.
                let task_id = snapshot.task_id.clone();
                let existing = self.items.iter_mut().find(|e| {
                    e.read(cx).kind().is_background_task_with_id(&task_id)
                });
                if let Some(entity) = existing {
                    entity.update(cx, |item, _| {
                        if let ConvItem::BackgroundTask(bt) = item.kind_mut() {
                            bt.status = snapshot.status;
                            bt.event_count = snapshot.event_count;
                            bt.total_bytes = snapshot.total_bytes;
                            bt.exit_code = snapshot.exit_code;
                            bt.failure_summary = snapshot.failure_summary.clone();
                            // Refresh recent events from the task's ring buffer.
                            if let Some(task) =
                                agent::background_task::get_by_str(&task_id)
                            {
                                bt.recent_events = task
                                    .recent_events()
                                    .iter()
                                    .filter_map(|e| match &e.event {
                                        agent::background_task::TaskEventKind::Output(t) => {
                                            Some(t.clone())
                                        }
                                        _ => None,
                                    })
                                    .take(20)
                                    .collect();
                            }
                        }
                    });
                } else {
                    // New task card.
                    let recent_events = if let Some(task) =
                        agent::background_task::get_by_str(&task_id)
                    {
                        task.recent_events()
                            .iter()
                            .filter_map(|e| match &e.event {
                                agent::background_task::TaskEventKind::Output(t) => {
                                    Some(t.clone())
                                }
                                _ => None,
                            })
                            .take(20)
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let ix = self.items.len();
                    let entity = cx.new(|_| {
                        MessageItem::new(
                            ConvItem::BackgroundTask(BackgroundTaskItem {
                                task_id: snapshot.task_id.clone(),
                                kind: snapshot.kind,
                                description: snapshot.description.clone(),
                                status: snapshot.status,
                                event_count: snapshot.event_count,
                                total_bytes: snapshot.total_bytes,
                                exit_code: snapshot.exit_code,
                                failure_summary: snapshot.failure_summary.clone(),
                                created_at: Some(Instant::now()),
                                recent_events,
                            }),
                            role.to_string(),
                            ix,
                            weak,
                        )
                    });
                    self.items.push(entity);
                }
                // The workspace's event subscriber re-renders on the emitted event.
            }
        }
    }

    /// Drop the trailing item if it is a stale `Retry` badge, so the real
    /// content event that follows pushes its own item into the freed slot.
    fn pop_trailing_retry(&mut self, cx: &App) {
        if self
            .items
            .last()
            .is_some_and(|e| matches!(e.read(cx).kind(), ConvItem::Retry { .. }))
        {
            self.items.pop();
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Rebuild view state from a `Thread`'s canonical message list (used when loading a historical thread).
    ///
    /// `notes` are the persisted UI annotations (`Error` / `Notice`) that live
    /// outside the canonical message list — they are spliced back at the end of
    /// the turn they belong to (anchored by user-message id), so a reloaded
    /// thread reproduces what the user saw without the model request ever
    /// learning they exist. The request prefix is untouched.
    pub fn rebuild_from_messages(
        messages: &[Message],
        usage: &std::collections::HashMap<String, TokenUsage>,
        role: &str,
        running: bool,
        notes: &[UiNoteRecord],
        ctx: ApplyCtx,
        cx: &mut App,
    ) -> Self {
        let ApplyCtx { weak, cwd } = ctx;
        let plain = build_items(messages, usage, running);
        let merged = merge_ui_notes(messages, plain, notes);
        let items = merged
            .into_iter()
            .enumerate()
            .map(|(id, kind)| {
                cx.new(|cx| {
                    let text = match &kind {
                        ConvItem::Assistant { text, .. } => Some(text.clone()),
                        _ => None,
                    };
                    let mut item = MessageItem::new(kind, role.to_string(), id, weak.clone());
                    // For rebuilt (non-streaming) text items, do a full parse
                    // + finalize so blocks are populated and the frozen prefix
                    // is the entire document (no further updates expected).
                    if let Some(text) = text {
                        item.update_text(&text, cx);
                        item.finalize_parser(cx);
                    }
                    // Mount + finalize persistent markdown for every historical
                    // reasoning round inside a `Thinking` segment, so selection
                    // works on reloaded history (not just live-streamed turns).
                    item.rebuild_activity_reasoning(cx);
                    // Mount the persistent `TerminalPanel` for every historical
                    // tool call (activity-segment entries + top-level ToolCall)
                    // so reloaded history renders the terminal-styled body with
                    // working selection, not a per-frame fallback.
                    item.rebuild_tool_panels(cwd.clone(), cx);
                    item
                })
            })
            .collect();
        Self {
            items,
            turn_started_at: Instant::now(),
        }
    }

    /// Rehydrate persisted background-task cards after rebuilding canonical
    /// messages. Running/Stopping snapshots have already been normalized to
    /// SessionEnded by `Thread::restore`.
    pub fn restore_background_tasks(
        &mut self,
        snapshots: &[agent::background_task::TaskSnapshot],
        role: &str,
        weak: WeakEntity<Workspace>,
        cx: &mut App,
    ) {
        for snapshot in snapshots {
            let id = self.items.len();
            self.items.push(cx.new(|_| {
                MessageItem::new(
                    ConvItem::BackgroundTask(BackgroundTaskItem {
                        task_id: snapshot.task_id.clone(),
                        kind: snapshot.kind,
                        description: snapshot.description.clone(),
                        status: snapshot.status,
                        event_count: snapshot.event_count,
                        total_bytes: snapshot.total_bytes,
                        exit_code: snapshot.exit_code,
                        failure_summary: snapshot.failure_summary.clone(),
                        created_at: None,
                        recent_events: Vec::new(),
                    }),
                    role.to_string(),
                    id,
                    weak.clone(),
                )
            }));
        }
    }
}

/// Splice persisted UI notes back into the rebuilt canonical item list.
///
/// A note is anchored to the user message whose turn it belongs to. The
/// canonical `items` list has one `ConvItem::User` bubble per text/image-
/// bearing user message (in message order), so those bubbles align 1:1 with
/// the "segment anchors" derived from `messages`. Each note lands at the end
/// of its segment — i.e. right before the next turn's user bubble — mirroring
/// where it appeared live. Notes whose anchor was a pure-tool-result user
/// message (no bubble) fold into the nearest preceding segment; notes whose
/// anchor was dropped by compaction land at the tail; notes emitted before
/// any user message (anchor `None`) land at the top.
///
/// Notes arrive already sorted by `seq` from `list_ui_notes`, so per-segment
/// order preserves emit order with no extra sort.
fn merge_ui_notes(
    messages: &[Message],
    items: Vec<ConvItem>,
    notes: &[UiNoteRecord],
) -> Vec<ConvItem> {
    if notes.is_empty() {
        return items;
    }

    // Segment anchors: user messages that produce a User bubble, in message
    // order. These align 1:1 with the User bubbles in `items` because
    // `build_items` pushes exactly one User bubble per such message and none
    // otherwise. The predicate mirrors `build_items` exactly: a bubble is
    // pushed iff the message has a non-empty Text/Thinking join or any Image.
    let segment_ids: Vec<&str> = messages
        .iter()
        .filter(|m| {
            if m.role != Role::User {
                return false;
            }
            if m.ui
                .as_ref()
                .and_then(|ui| ui.external_event)
                .unwrap_or(false)
            {
                return false;
            }
            let has_text = m.content.iter().any(|c| match c {
                MessageContent::Text(t) | MessageContent::Thinking { text: t, .. } => !t.is_empty(),
                _ => false,
            });
            let has_image = m
                .content
                .iter()
                .any(|c| matches!(c, MessageContent::Image { .. }));
            has_text || has_image
        })
        .map(|m| m.id.as_str())
        .collect();

    // Message index of every user message (including pure-tool-result ones),
    // so a note anchored to a no-bubble user message can be folded into the
    // nearest preceding segment instead of orphaning.
    let user_msg_index: HashMap<&str, usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, m)| (m.id.as_str(), i))
        .collect();
    let segment_msg_ix: Vec<usize> = segment_ids.iter().map(|id| user_msg_index[id]).collect();

    // User-bubble positions in `items`, aligned with `segment_ids` by order.
    let bubble_ix: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches!(it, ConvItem::User { .. }))
        .map(|(i, _)| i)
        .collect();

    // The 1:1 alignment between segment anchors (from messages) and User
    // bubbles (from items) is what lets a note be placed at its turn's end.
    // If these ever diverge — e.g. `build_items` changes its bubble rule —
    // placement would silently misfire, so assert in dev builds and warn in
    // release: a crash on misplacement is worse than a logged divergence, but
    // silence is worse than either.
    if segment_ids.len() != bubble_ix.len() {
        tracing::warn!(
            anchors = segment_ids.len(),
            bubbles = bubble_ix.len(),
            "merge_ui_notes: segment anchors and User bubbles diverged; \
             UI-note placement may be wrong until build_items is realigned"
        );
    }
    debug_assert_eq!(
        segment_ids.len(),
        bubble_ix.len(),
        "segment anchors and User bubbles diverged: build_items and merge_ui_notes disagree"
    );

    // Bucket each note by its target segment.
    let mut buckets: Vec<Vec<&UiNoteRecord>> = (0..segment_ids.len()).map(|_| Vec::new()).collect();
    let mut top: Vec<&UiNoteRecord> = Vec::new();
    let mut orphan: Vec<&UiNoteRecord> = Vec::new();
    for n in notes {
        match &n.anchor_user_id {
            None => top.push(n),
            Some(aid) => {
                if let Some(k) = segment_ids.iter().position(|id| *id == aid.as_str()) {
                    buckets[k].push(n);
                } else if let Some(&mi) = user_msg_index.get(aid.as_str()) {
                    // Anchor is a no-bubble user message (e.g. a tool-result
                    // message mid-loop): fold into the nearest preceding segment.
                    let seg = segment_msg_ix
                        .iter()
                        .enumerate()
                        .filter(|(_, smi)| **smi <= mi)
                        .map(|(k, _)| k)
                        .next_back();
                    match seg {
                        Some(k) => buckets[k].push(n),
                        None => top.push(n),
                    }
                } else {
                    // Anchor references a message compaction dropped — tail it.
                    orphan.push(n);
                }
            }
        }
    }

    let mut out: Vec<ConvItem> = Vec::with_capacity(items.len() + notes.len());
    let first_bubble = bubble_ix.first().copied().unwrap_or(items.len());
    for n in &top {
        out.push(note_to_item(n));
    }
    // Canonical items before the first User bubble (a no-user-message prefix,
    // normally empty since conversations start with a user message).
    for it in items.iter().take(first_bubble) {
        out.push(it.clone());
    }
    for (k, &start) in bubble_ix.iter().enumerate() {
        let end = bubble_ix.get(k + 1).copied().unwrap_or(items.len());
        for it in items.iter().take(end).skip(start) {
            out.push(it.clone());
        }
        for n in &buckets[k] {
            out.push(note_to_item(n));
        }
    }
    for n in &orphan {
        out.push(note_to_item(n));
    }
    out
}

/// Render a persisted note as its live `ConvItem` counterpart, reading the
/// `text` payload from `data`.
fn note_to_item(n: &UiNoteRecord) -> ConvItem {
    let text = n
        .data
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match n.kind {
        UiNoteKind::Error => ConvItem::Error(text),
        UiNoteKind::Notice => ConvItem::Notice(text),
        UiNoteKind::PlanReview => ConvItem::PlanReview {
            plan_text: text,
            active: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::Message;
    use agent::language_model::{
        LanguageModelToolResult, LanguageModelToolUse, MessageContent, Role,
    };
    use std::sync::Arc;

    #[test]
    fn optimistic_steer_rolls_back_and_late_confirmation_heals_tombstone() {
        let cx = gpui::TestAppContext::single();
        let conversation = cx.update(|cx| cx.new(|_| ConversationState::new()));
        let meta = UserTurnMeta::new(1, "test-model".into(), None);

        cx.update(|cx| {
            conversation.update(cx, |conversation, cx| {
                conversation.push_pending_steer(
                    "please adjust".into(),
                    Vec::new(),
                    meta,
                    "steer-1".into(),
                    gpui::WeakEntity::<Workspace>::new_invalid(),
                    cx,
                );
                assert!(conversation.rollback_pending_steer("steer-1", cx));
                assert!(conversation.is_empty(cx));
                assert!(conversation.confirm_pending_steer("steer-1", cx));
                assert!(!conversation.is_empty(cx));
            });
        });

        cx.update(|cx| {
            conversation.read_with(cx, |conversation, cx| {
                let item = conversation.items[0].read(cx);
                let ConvItem::User {
                    meta,
                    display_state,
                    ..
                } = item.kind()
                else {
                    panic!("expected user item");
                };
                assert_eq!(display_state, &UserMessageDisplayState::Normal);
                assert!(meta.as_ref().is_some_and(|meta| meta.steered));
            });
        });
    }

    /// Build a message with a chosen id (Message::user randomizes it, which
    /// defeats anchor-based placement tests).
    fn msg_with_id(id: &str, role: Role, text: &str) -> Message {
        Message {
            id: id.to_string(),
            timestamp: 0,
            parent_id: None,
            role,
            content: vec![MessageContent::Text(text.to_string())],
            ui: None,
        }
    }

    fn note(seq: i64, kind: UiNoteKind, anchor: Option<&str>, text: &str) -> UiNoteRecord {
        UiNoteRecord {
            id: seq,
            thread_id: "t".to_string(),
            seq,
            anchor_user_id: anchor.map(str::to_owned),
            kind,
            data: serde_json::json!({ "text": text }),
            ts: 0,
        }
    }

    /// A flat signature of each merged item, in order, for readable assertions.
    fn signature(items: &[ConvItem]) -> Vec<String> {
        items
            .iter()
            .map(|it| match it {
                ConvItem::User { text, .. } => format!("U:{text}"),
                ConvItem::Assistant { text, .. } => format!("A:{text}"),
                ConvItem::Notice(t) => format!("N:{t}"),
                ConvItem::Error(t) => format!("E:{t}"),
                ConvItem::PlanReview { plan_text, .. } => format!("P:{plan_text}"),
                _ => "?".to_string(),
            })
            .collect()
    }

    /// Persisted Error/Notice cards are spliced back at the end of their owning
    /// turn; None-anchor notes top the list; notes whose anchor was dropped by
    /// compaction land at the tail.
    #[test]
    fn merge_ui_notes_places_notes_at_turn_end() {
        let messages = vec![
            msg_with_id("u1", Role::User, "hello"),
            msg_with_id("a1", Role::Assistant, "hi"),
            msg_with_id("u2", Role::User, "again"),
            msg_with_id("a2", Role::Assistant, "yo"),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        // Notes arrive seq-sorted from list_ui_notes.
        let notes = vec![
            note(1, UiNoteKind::Notice, None, "top"),
            note(2, UiNoteKind::Notice, Some("u1"), "t0end"),
            note(3, UiNoteKind::Error, Some("u2"), "t1end"),
            note(4, UiNoteKind::Notice, Some("ghost"), "orphan"),
        ];
        let merged = merge_ui_notes(&messages, items, &notes);
        assert_eq!(
            signature(&merged),
            vec![
                "N:top",   // None-anchor → top
                "U:hello", // turn 0
                "A:hi", "N:t0end", // anchor u1 → end of turn 0
                "U:again", // turn 1
                "A:yo", "E:t1end",  // anchor u2 → end of turn 1
                "N:orphan", // unknown anchor → tail
            ]
        );
    }

    /// A note anchored to a pure-tool-result user message (no User bubble)
    /// folds into the nearest preceding segment rather than orphaning.
    #[test]
    fn merge_ui_notes_folds_no_bubble_anchor_into_preceding_turn() {
        // Turn 0: user prompt + assistant; then a tool-result user message
        // (no text → no bubble) + assistant reply. A note anchored to the
        // tool-result user message should land at the end of turn 0.
        let mut tr = msg_with_id("u2", Role::User, "");
        tr.content = vec![MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: "tu_1".to_string(),
            tool_name: Arc::from("Read"),
            is_error: false,
            content: "done".to_string(),
        })];
        let messages = vec![
            msg_with_id("u1", Role::User, "do it"),
            msg_with_id("a1", Role::Assistant, "ok"),
            tr,
            msg_with_id("a2", Role::Assistant, "done"),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        // No second User bubble — both assistant replies belong to turn 0.
        assert_eq!(
            items
                .iter()
                .filter(|i| matches!(i, ConvItem::User { .. }))
                .count(),
            1
        );
        let notes = vec![note(1, UiNoteKind::Notice, Some("u2"), "mid")];
        let merged = merge_ui_notes(&messages, items, &notes);
        assert_eq!(
            signature(&merged).last(),
            Some(&"N:mid".to_string()),
            "no-bubble anchor folds to the nearest preceding segment's tail"
        );
    }

    /// A dismissed plan persists as a `PlanReview` note anchored to the user
    /// message that triggered the plan's turn. The rebuild must splice the
    /// collapsed plan card back at the end of that turn — ahead of the
    /// dismissing user message — so a switched-away-and-back thread reproduces
    /// the live order rather than silently dropping the plan.
    #[test]
    fn merge_ui_notes_places_dismissed_plan_before_dismissing_message() {
        // u1 triggers the plan turn (a1 proposes a plan); u2 is the free-form
        // message that dismissed it; a2 is the re-proposal turn.
        let messages = vec![
            msg_with_id("u1", Role::User, "plan it"),
            msg_with_id("a1", Role::Assistant, "here is the plan"),
            msg_with_id("u2", Role::User, "revise step 2"),
            msg_with_id("a2", Role::Assistant, "revised"),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        // Anchored to u1 — the plan turn's triggering message — so the card
        // lands at the end of turn 0, before the u2 bubble that dismissed it.
        let notes = vec![note(1, UiNoteKind::PlanReview, Some("u1"), "PLAN BODY")];
        let merged = merge_ui_notes(&messages, items, &notes);
        assert_eq!(
            signature(&merged),
            vec![
                "U:plan it", // turn 0
                "A:here is the plan",
                "P:PLAN BODY",     // dismissed plan → end of turn 0
                "U:revise step 2", // the dismissing message
                "A:revised",
            ]
        );
        // The rebuilt card is the inactive record form (no verdict buttons).
        assert!(matches!(
            merged[2],
            ConvItem::PlanReview { active: false, .. }
        ));
    }

    /// A tool_result in a user message must pair back to the ToolUse emitted in the
    /// preceding assistant message, so a reloaded historical thread shows tool output.
    #[test]
    fn rebuild_pairs_tool_result_in_user_message() {
        let messages = vec![
            Message::user("read the file".to_string()),
            Message::assistant(vec![
                MessageContent::Text("let me read it".to_string()),
                MessageContent::ToolUse(LanguageModelToolUse {
                    id: "tu_1".to_string(),
                    name: Arc::from("Read"),
                    raw_input: String::new(),
                    input: serde_json::Value::Null,
                    is_input_complete: true,
                    thought_signature: None,
                }),
            ]),
            Message::user_with_content(vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_1".to_string(),
                tool_name: Arc::from("Read"),
                is_error: false,
                content: "file contents here".to_string(),
            })]),
        ];
        let items = build_items(&messages, &std::collections::HashMap::new(), false);
        let tool = find_thinking_entry(&items, "tu_1").expect("tool call entry present");
        assert_eq!(tool.output, "file contents here");
        assert_eq!(tool.status, ToolCallStatus::Success);
        assert!(!tool.is_error);
        assert!(
            !items
                .iter()
                .any(|i| matches!(i, ConvItem::User { text, .. } if text.is_empty()))
        );
    }

    #[test]
    fn rebuild_pairs_error_tool_result() {
        let messages = vec![Message::user_with_content(vec![
            MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_x".to_string(),
                tool_name: Arc::from("Bash"),
                is_error: true,
                content: "boom".to_string(),
            }),
        ])];
        let items = build_items(&messages, &std::collections::HashMap::new(), false);
        let tool = find_thinking_entry(&items, "tu_x").expect("standalone result entry present");
        assert_eq!(tool.output, "boom");
        assert_eq!(tool.status, ToolCallStatus::Error);
        assert!(tool.is_error);
        assert_eq!(tool.name, "Bash");
    }

    /// Locate a tool-call entry by id within any `ThinkingContainer`. Used by
    /// rebuild tests that assert against batched entries instead of top-level
    /// `ToolCall` items.
    fn find_thinking_entry<'a>(items: &'a [ConvItem], id: &str) -> Option<&'a ToolCallItem> {
        items.iter().find_map(|i| match i {
            ConvItem::Thinking(t) => t.entries.iter().find_map(|e| match e {
                ActivityEntry::Tool(tool) if tool.id == id => Some(tool),
                _ => None,
            }),
            _ => None,
        })
    }

    #[test]
    fn rebuild_restores_agent_compact_metadata() {
        let sub_messages = vec![
            Message::user("research the foo module".to_string()),
            Message::assistant(vec![MessageContent::Text("found 3 files".to_string())]),
        ];
        let envelope = serde_json::json!({
            "final": "found 3 files",
            "messages": sub_messages,
        })
        .to_string();
        let messages = vec![
            Message::assistant(vec![MessageContent::ToolUse(LanguageModelToolUse {
                id: "tu_agent".to_string(),
                name: Arc::from("Agent"),
                raw_input: String::new(),
                input: serde_json::json!({
                    "subagent_type": "researcher",
                    "description": "Inspect foo module",
                    "prompt": "research foo"
                }),
                is_input_complete: true,
                thought_signature: None,
            })]),
            Message::user_with_content(vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_agent".to_string(),
                tool_name: Arc::from("Agent"),
                is_error: false,
                content: envelope,
            })]),
        ];
        let items = build_items(&messages, &std::collections::HashMap::new(), false);
        let task = items
            .iter()
            .find_map(|i| match i {
                ConvItem::AgentTask(t) if t.id == "tu_agent" => Some(t),
                _ => None,
            })
            .expect("agent task item present");
        assert_eq!(task.subagent_type, "researcher");
        assert_eq!(task.description, "Inspect foo module");
        assert_eq!(task.status, ToolCallStatus::Success);
        assert!(!task.is_error);
    }

    /// A legacy `agent` tool result (plain text, no JSON envelope) must still
    /// render its final text without panicking.
    #[test]
    fn agent_final_text_falls_back_for_legacy_content() {
        assert_eq!(
            agent::tools::agent::agent_final_text("just a plain summary"),
            "just a plain summary"
        );
        assert_eq!(
            agent::tools::agent::agent_final_text("not json { at all"),
            "not json { at all"
        );
        assert!(agent::tools::agent::agent_sub_messages("plain text").is_none());
    }

    /// Multiple ToolUse blocks in one assistant response (a parallel batch)
    /// rebuild as a single folded `ThinkingContainer` with one entry per call —
    /// the live `apply` invariant that all of a response's tools share a batch.
    /// Text flanking the batch becomes its own `Assistant` item on each side.
    #[test]
    fn rebuild_batches_parallel_tools_into_one_container() {
        let messages = vec![
            Message::user("go".to_string()),
            Message::assistant(vec![
                MessageContent::Text("opening two files".to_string()),
                MessageContent::ToolUse(LanguageModelToolUse {
                    id: "tu_a".to_string(),
                    name: Arc::from("Read"),
                    raw_input: String::new(),
                    input: serde_json::Value::Null,
                    is_input_complete: true,
                    thought_signature: None,
                }),
                MessageContent::ToolUse(LanguageModelToolUse {
                    id: "tu_b".to_string(),
                    name: Arc::from("Read"),
                    raw_input: String::new(),
                    input: serde_json::Value::Null,
                    is_input_complete: true,
                    thought_signature: None,
                }),
            ]),
            Message::user_with_content(vec![
                MessageContent::ToolResult(LanguageModelToolResult {
                    tool_use_id: "tu_a".to_string(),
                    tool_name: Arc::from("Read"),
                    is_error: false,
                    content: "a".to_string(),
                }),
                MessageContent::ToolResult(LanguageModelToolResult {
                    tool_use_id: "tu_b".to_string(),
                    tool_name: Arc::from("Read"),
                    is_error: false,
                    content: "b".to_string(),
                }),
            ]),
        ];
        let items = build_items(&messages, &std::collections::HashMap::new(), false);
        // Exactly one Thinking container, holding both calls in order.
        let containers: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                ConvItem::Thinking(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(containers.len(), 1, "one batch → one container");
        let t = containers[0];
        assert!(!t.streaming);
        assert!(t.collapsed, "historical container auto-folds");
        assert_eq!(t.entries.len(), 2);
        let (ActivityEntry::Tool(e0), ActivityEntry::Tool(e1)) = (&t.entries[0], &t.entries[1])
        else {
            panic!("expected tool entries");
        };
        assert_eq!(e0.id, "tu_a");
        assert_eq!(e0.output, "a");
        assert_eq!(e1.id, "tu_b");
        assert_eq!(e1.output, "b");
        // Prose precedes the container.
        assert!(matches!(items.first(), Some(ConvItem::User { .. })));
    }

    /// A still-running thread rebuilds with its trailing assistant bubble marked
    /// `streaming` so resumed `AgentText` deltas append to it instead of opening
    /// a second bubble (Bug 2). The completed path stays non-streaming.
    #[test]
    fn build_items_trailing_streaming_marks_running_tail() {
        let messages = vec![
            Message::user("hello".to_string()),
            Message::assistant(vec![MessageContent::Text("draft reply".to_string())]),
        ];
        let completed = build_items(&messages, &std::collections::HashMap::new(), false);
        match completed.last().unwrap() {
            ConvItem::Assistant { streaming, .. } => {
                assert!(!*streaming, "completed tail not streaming")
            }
            _ => panic!("trailing item is an assistant bubble"),
        }
        let running = build_items(&messages, &std::collections::HashMap::new(), true);
        match running.last().unwrap() {
            ConvItem::Assistant { streaming, .. } => {
                assert!(*streaming, "running tail is streaming")
            }
            _ => panic!("trailing item is an assistant bubble"),
        }
    }

    /// `StopReason::ToolUse` does NOT freeze the activity segment: after a
    /// ToolUse stop, `accepting_entries` stays true and `streaming` stays
    /// true so the next model response's tool calls fold into the same
    /// segment. Only a terminal stop (`EndTurn`) freezes it. This exercises
    /// the `ThinkingContainer` state transitions that the `Stop` arm drives
    /// via `finalize_streaming(terminal)` + `recompute_streaming`.
    #[test]
    fn tool_use_stop_does_not_freeze_segment() {
        let mut t = ThinkingContainer::new();
        // A streaming reasoning round (the model thought before emitting the
        // tool call) plus the terminal tool entry.
        t.entries.push(ActivityEntry::Reasoning {
            text: "round 1".into(),
            streaming: true,
            collapsed: false,
            user_toggled: false,
            markdown: None,
        });
        t.entries.push(ActivityEntry::Tool(ToolCallItem {
            id: "1".into(),
            name: "Read".into(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            input: serde_json::Value::Null,
            streaming: false,
            collapsed: false,
            user_toggled: false,
            panel: None,
        }));
        // All entries terminal, but segment is still accepting (turn in progress).
        t.recompute_streaming();
        assert!(t.accepting_entries, "segment still accepting entries");
        assert!(t.streaming, "segment stays live while accepting entries");
        assert!(t.frozen_secs.is_none(), "elapsed not pinned mid-turn");

        // Simulate the Stop(ToolUse) path: `finalize_streaming(false)` runs
        // `finalize_reasoning_rounds` — the round's `streaming` flips off so
        // the next `AgentThinking` opens a fresh round instead of appending to
        // this one — but the segment stays open for the next response's tool
        // calls. (The full `MessageItem::finalize_streaming` needs an entity;
        // we exercise the segment-level invariant directly.)
        t.finalize_reasoning_rounds();
        assert!(
            t.last_streaming_reasoning_index().is_none(),
            "reasoning round closed; next AgentThinking starts a fresh round"
        );
        assert!(
            t.accepting_entries,
            "segment still accepting after ToolUse stop"
        );
        assert!(t.streaming, "segment stays live while accepting entries");
        assert!(t.frozen_secs.is_none(), "elapsed not pinned mid-turn");

        // Now the terminal stop path: finalize_segment freezes.
        t.finalize_segment();
        t.recompute_streaming();
        assert!(!t.accepting_entries, "segment closed on terminal stop");
        assert!(!t.streaming, "segment frozen on terminal stop");
        assert!(t.frozen_secs.is_some(), "elapsed pinned on terminal stop");
    }
    /// `close_for_text` stops accepting entries (so subsequent
    /// `AgentThinking` opens a fresh segment), finalizes any still-streaming
    /// reasoning rounds (idempotent with `finalize_reasoning_rounds`, which a
    /// mid-turn `Stop(ToolUse)` already ran), and pins the elapsed timer — but
    /// does NOT auto-collapse (mid-turn, the user may be inspecting the tree).
    /// This is the live-path mirror of `build_items`'s `close_segment` on
    /// `MessageContent::Text` (issue #216).
    #[test]
    fn close_for_text_stops_accepting_entries() {
        let mut t = ThinkingContainer::new();
        t.entries.push(ActivityEntry::Reasoning {
            text: "round 1".into(),
            streaming: true, // still-live if finalize_reasoning_rounds was skipped
            collapsed: false,
            user_toggled: false,
            markdown: None,
        });
        t.entries.push(ActivityEntry::Tool(ToolCallItem {
            id: "1".into(),
            name: "Read".into(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            input: serde_json::Value::Null,
            streaming: false,
            collapsed: false,
            user_toggled: false,
            panel: None,
        }));
        t.recompute_streaming();
        assert!(t.accepting_entries);
        assert!(t.streaming, "segment live while reasoning streaming");

        t.close_for_text();
        assert!(!t.accepting_entries, "segment closed for text");
        assert!(!t.streaming, "segment not streaming after close");
        assert!(t.frozen_secs.is_some(), "elapsed pinned");
        // Reasoning entry finalized → last_streaming_reasoning_index returns None.
        assert!(
            t.last_streaming_reasoning_index().is_none(),
            "reasoning finalized, new round would start"
        );
    }

    /// `finalize_reasoning_rounds` (the segment-level half of a mid-turn
    /// `Stop(ToolUse)`) flips every streaming reasoning round's `streaming`
    /// flag off so the next `AgentThinking` opens a fresh round, WITHOUT
    /// closing the segment — `accepting_entries` stays true, `streaming` stays
    /// true, and the elapsed timer stays unpinned.
    #[test]
    fn finalize_reasoning_rounds_closes_round_keeps_segment_open() {
        let mut t = ThinkingContainer::new();
        t.entries.push(ActivityEntry::Reasoning {
            text: "round 1".into(),
            streaming: true,
            collapsed: false,
            user_toggled: false,
            markdown: None,
        });
        t.entries.push(ActivityEntry::Tool(ToolCallItem {
            id: "1".into(),
            name: "Read".into(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            input: serde_json::Value::Null,
            streaming: false,
            collapsed: false,
            user_toggled: false,
            panel: None,
        }));
        t.recompute_streaming();
        assert!(t.streaming, "segment live while reasoning streaming");

        t.finalize_reasoning_rounds();

        // The reasoning round is closed: no streaming round remains, so the
        // next AgentThinking starts a fresh round instead of appending here.
        assert!(
            t.last_streaming_reasoning_index().is_none(),
            "streaming round finalized"
        );
        match &t.entries[0] {
            ActivityEntry::Reasoning { streaming, .. } => {
                assert!(!*streaming, "round's streaming flag flipped off");
            }
            _ => panic!("first entry is not reasoning"),
        }
        // The segment itself stays open for the next model response's tool
        // calls: accepting_entries/streaming stay true, elapsed not pinned.
        assert!(t.accepting_entries, "segment still accepting entries");
        assert!(t.streaming, "segment stays live while accepting entries");
        assert!(t.frozen_secs.is_none(), "elapsed not pinned mid-turn");

        // Idempotent: a second call is a no-op.
        t.finalize_reasoning_rounds();
        assert!(t.accepting_entries);
        assert!(t.streaming);
        assert!(t.frozen_secs.is_none());
    }

    /// `last_streaming_reasoning_index` returns the index of the last streaming
    /// reasoning entry, or `None` when no reasoning is active.
    #[test]
    fn last_streaming_reasoning_index_finds_active_round() {
        let mut t = ThinkingContainer::new();
        assert!(t.last_streaming_reasoning_index().is_none());

        // Push a non-streaming reasoning entry.
        t.entries.push(ActivityEntry::Reasoning {
            text: "done".into(),
            streaming: false,
            collapsed: true,
            user_toggled: false,
            markdown: None,
        });
        assert!(t.last_streaming_reasoning_index().is_none());

        // Push a streaming reasoning entry.
        t.entries.push(ActivityEntry::Reasoning {
            text: "active".into(),
            streaming: true,
            collapsed: false,
            user_toggled: false,
            markdown: None,
        });
        assert_eq!(t.last_streaming_reasoning_index(), Some(1));

        // A tool entry after it does not affect the search.
        t.entries.push(ActivityEntry::Tool(ToolCallItem {
            id: "t1".into(),
            name: "Bash".into(),
            title: String::new(),
            status: ToolCallStatus::Running,
            output: String::new(),
            is_error: false,
            input: serde_json::Value::Null,
            streaming: true,
            collapsed: false,
            user_toggled: false,
            panel: None,
        }));
        // Still finds the streaming reasoning at index 1.
        assert_eq!(t.last_streaming_reasoning_index(), Some(1));
    }

    /// `get_tool_entry_mut` finds tool entries by id and skips reasoning entries.
    #[test]
    fn get_tool_entry_mut_skips_reasoning() {
        let mut t = ThinkingContainer::new();
        t.entries.push(ActivityEntry::Reasoning {
            text: "thinking".into(),
            streaming: false,
            collapsed: true,
            user_toggled: false,
            markdown: None,
        });
        t.entries.push(ActivityEntry::Tool(ToolCallItem {
            id: "tu_1".into(),
            name: "Read".into(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            input: serde_json::Value::Null,
            streaming: false,
            collapsed: false,
            user_toggled: false,
            panel: None,
        }));
        assert!(t.get_tool_entry_mut("tu_1").is_some());
        assert!(t.get_tool_entry_mut("nonexistent").is_none());
    }
}
