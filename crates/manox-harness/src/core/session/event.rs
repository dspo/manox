// The event vocabulary of the session log (Session Core, format v1).
//
// One serde vocabulary is the whole durable schema: the log line IS the
// wire form of an `Event` wrapped in an `Envelope`. The AHP projection
// translates events outward; nothing outside that translation layer may
// build a second vocabulary. Two rules hold for every future addition:
// a user-visible fact that must survive a restart is an event, and an
// identity field never appears in an event payload — events carry id
// references only, identity lives in the immutable header.
//
// The conversation domain (deltas, tool calls, turns) joins this enum when
// the engine's writers are re-homed onto it; its row names are unchanged
// from the previous journal, so the move is mechanical. This file starts
// with the domains that have no legacy shape to inherit: session facts,
// swarm lifecycle, and system notes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::session::TurnOwner;
use crate::types::{AgentMessage, Usage};

/// The budgets armed on a swarm run at dispatch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Budgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<u64>,
}

/// Every way a swarm run can end. The complete set is deliberate: a run
/// never ends silently, and no two kinds collapse into one another on the
/// wire — the "timed out" of the previous model, which rode a string
/// prefix inside a generic failure, is a first-class kind here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SettlementKind {
    Completed,
    TimedOut,
    Stalled,
    Aborted,
    Error,
    Refusal,
    MaxTokens,
}

/// The whole durable vocabulary of a session log: one serde enum is the
/// single schema (absorbed from the former `SessionTreeEntry`; that name
/// lives on as a type alias while call sites migrate). Wire names are the
/// row names — the conversation domain keeps its historical names, and the
/// swarm/disposal rows join them under the same tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "message", rename_all = "camelCase")]
    Message {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        message: AgentMessage,
        /// The RPC id this message was submitted under (§C.2 `originRpc`,
        /// source RPC id). The server pins the client's Submit
        /// `origin_rpc` on the user message's journal entry so the client can
        /// retire its optimistic echo (echo/retire protocol, §F.2). Absent on
        /// every other entry and on older session files.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
    #[serde(rename = "compaction", rename_all = "camelCase")]
    Compaction {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        summary: String,
        first_kept_entry_id: Option<String>,
        tokens_before: u64,
        /// Materialized messages kept after the boundary — a self-contained
        /// checkpoint: a context rebuild reads them straight from the entry
        /// instead of walking the tree from `first_kept_entry_id`. Absent on
        /// older session files, where the tree walk remains the fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retained_tail: Option<Vec<AgentMessage>>,
        /// Token usage reported by the summarization call, when recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Structured payload a summarization hook may attach.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        /// Whether the boundary was written by a hook rather than the
        /// harness's own compaction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    /// A change of model for the following turns.
    #[serde(rename = "model_change", rename_all = "camelCase")]
    ModelChange {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        provider: String,
        model_id: String,
    },
    /// A change in reasoning depth for the following turns.
    #[serde(rename = "thinking_level_change", rename_all = "camelCase")]
    ThinkingLevelChange {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        /// The reasoning tier for following turns, or `"off"` to disable it.
        /// Always a string — `null` would drop the field on the wire.
        thinking_level: String,
    },
    /// A change of the working directory the following tool calls run in.
    ///
    /// The header cwd is immutable (append-only file), so this entry is the
    /// durable witness of the per-call sticky cwd advancing — resolution chain
    /// `explicit cwd argument → sticky → header cwd`. Restore projects the
    /// latest one; tools do not project it into the transcript.
    #[serde(rename = "cwd_change", rename_all = "camelCase")]
    CwdChange {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        cwd: String,
    },
    /// A change in the set of tools mounted for the following turns.
    #[serde(rename = "active_tools_change", rename_all = "camelCase")]
    ActiveToolsChange {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        active_tool_names: Vec<String>,
    },
    /// A conversation- or branch-level summary produced by branch summarization.
    /// Flat (not nested): `summary` is the prose string, `details` carries any
    /// structured payload (e.g. files changed).
    #[serde(rename = "branch_summary", rename_all = "camelCase")]
    BranchSummary {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    /// Extension entry whose payload the harness does not interpret.
    #[serde(rename = "custom", rename_all = "camelCase")]
    Custom {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<JsonValue>,
    },
    /// An extension message whose payload the harness does not interpret.
    /// `content` is a plain string or an array of text/image blocks, matching
    /// the v3 schema.
    #[serde(rename = "custom_message", rename_all = "camelCase")]
    CustomMessage {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        custom_type: String,
        #[serde(default, deserialize_with = "crate::types::deserialize_content_blocks")]
        content: Vec<crate::types::ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        #[serde(default)]
        display: bool,
    },
    /// A short human-readable label attached to a point in the tree.
    #[serde(rename = "label", rename_all = "camelCase")]
    Label {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        target_id: String,
        // `label` is optional on the wire and omitted when unset;
        // skip-on-None keeps Rust output byte-identical to external writers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Free-form session metadata (agent identity, environment, config snapshot).
    #[serde(rename = "session_info", rename_all = "camelCase")]
    SessionInfo {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// A cursor move: the leaf points at `targetId` (an older entry) for
    /// branching. Appended by `set_leaf_id`; never the walk start.
    #[serde(rename = "leaf", rename_all = "camelCase")]
    Leaf {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        target_id: Option<String>,
    },
    // ── v4 journal vocabulary (architecture doc §C.2) ─────────────────────
    //
    // Everything below extends the durable session log to the full
    // "every observable state change is an entry" surface (L3). The envelope
    // keys (`seq`/`id`/`parentId`/`timestamp`/`type`) are exclusive: payload
    // fields never reuse them, so tool handles are `callId` and subagent
    // handles `agentId` (§C.1 exclusivity rule).
    /// A persisted UI note card (was the fire-and-forget AppendUiNote).
    #[serde(rename = "ui_note", rename_all = "camelCase")]
    UiNote {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        note: JsonValue,
    },
    /// A model turn started.
    #[serde(rename = "turn_start", rename_all = "camelCase")]
    TurnStart {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        /// The process that started the turn (wire mirror of the journal's
        /// `TurnOwner`): readers of the shared journal use its liveness to
        /// settle a dead owner's turn without touching a live one. Absent on
        /// rows written before the stamp.
        #[serde(default)]
        owner: Option<TurnOwner>,
    },
    /// A model turn finished.
    #[serde(rename = "turn_finish", rename_all = "camelCase")]
    TurnFinish {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        cancelled: bool,
        failed: bool,
        stranded_steer_ids: Vec<String>,
    },
    /// The loop stopped advancing.
    #[serde(rename = "stop", rename_all = "camelCase")]
    Stop {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        reason: Option<String>,
    },
    /// A provider retry was scheduled.
    #[serde(rename = "retry", rename_all = "camelCase")]
    Retry {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
        reason: String,
        detail: Option<String>,
    },
    /// A terminal error, flattened to its message (`anyhow` is not
    /// serializable).
    #[serde(rename = "error", rename_all = "camelCase")]
    ErrorEvent {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        message: String,
    },
    /// An assistant text delta (durable streaming chunk).
    #[serde(rename = "agent_text_delta", rename_all = "camelCase")]
    AgentTextDelta {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        delta: String,
    },
    /// An assistant thinking delta.
    #[serde(rename = "agent_thinking_delta", rename_all = "camelCase")]
    AgentThinkingDelta {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        delta: String,
    },
    /// A tool call announced / updated (`callId` per §C.1).
    #[serde(rename = "tool_call", rename_all = "camelCase")]
    ToolCall {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        call_id: String,
        name: String,
        title: String,
        status: String,
        input: Option<JsonValue>,
    },
    /// A tool result settled (`callId` per §C.1).
    #[serde(rename = "tool_result", rename_all = "camelCase")]
    ToolResult {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        call_id: String,
        output: String,
        is_error: bool,
    },
    /// A streaming chunk of a tool's output (`callId` per §C.1).
    #[serde(rename = "tool_output_chunk", rename_all = "camelCase")]
    ToolOutputChunk {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        call_id: String,
        chunk: String,
    },
    /// An event surfaced by a subagent child session (`agentId` per §C.1).
    #[serde(rename = "subagent_child", rename_all = "camelCase")]
    SubagentChild {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        agent_id: String,
        event: JsonValue,
    },
    /// A subagent progress tick (`agentId` per §C.1).
    #[serde(rename = "subagent_progress", rename_all = "camelCase")]
    SubagentProgress {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        agent_id: String,
        agent_type: String,
        tool_uses: u32,
        latest_activity: Option<String>,
        status: String,
    },
    /// The project binding changed (`None` unbinds).
    #[serde(rename = "project_change", rename_all = "camelCase")]
    ProjectChange {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        path: Option<String>,
    },
    /// The persisted plan snapshot changed.
    #[serde(rename = "plan_update", rename_all = "camelCase")]
    PlanUpdate {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        snapshot: JsonValue,
    },
    /// The session goal changed.
    #[serde(rename = "goal", rename_all = "camelCase")]
    Goal {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        goal: Option<JsonValue>,
    },
    /// The display title changed.
    #[serde(rename = "title", rename_all = "camelCase")]
    Title {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        title: String,
    },
    /// The browser-suite set changed.
    #[serde(rename = "browser_suites", rename_all = "camelCase")]
    BrowserSuites {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        suites: Vec<String>,
    },
    /// A background-task snapshot changed.
    #[serde(rename = "background_task", rename_all = "camelCase")]
    BackgroundTask {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        snapshot: JsonValue,
    },
    /// An approval request or decision (the `pending_auth` projection's fold
    /// source; `kind` is `"request" | "decision"`).
    #[serde(rename = "approval", rename_all = "camelCase")]
    Approval {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        kind: String,
        auth_id: String,
        payload: JsonValue,
    },
    /// A user-question request or decision (the question seam's own
    /// vocabulary; `kind` is `"request" | "decision"`). Folds the same
    /// pending-card badge as an approval, kept distinct so the approval
    /// audit never carries an interactive ask.
    #[serde(rename = "question", rename_all = "camelCase")]
    Question {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        kind: String,
        auth_id: String,
        payload: JsonValue,
    },
    /// A durable user annotation upserted through the annotations channel
    /// (`ahp-session:/<id>/annotations`): the AHP `Annotation` JSON rides the
    /// row verbatim — the kernel persists it without interpreting it, and the
    /// prompt builder injects the unresolved entries' texts into the model
    /// context (latest-wins per annotation id).
    #[serde(rename = "annotation_set", rename_all = "camelCase")]
    AnnotationSet {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        annotation: serde_json::Value,
    },
    /// A durable user annotation removal — the fold collapses the named id.
    #[serde(rename = "annotation_removed", rename_all = "camelCase")]
    AnnotationRemoved {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        annotation_id: String,
    },
    /// Pin / archive flags changed.
    #[serde(rename = "pinned_archived", rename_all = "camelCase")]
    PinnedArchived {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        pinned: bool,
        archived: bool,
    },
    /// A compaction began (spinner state ahead of the `compaction` boundary).
    #[serde(rename = "compaction_started", rename_all = "camelCase")]
    CompactionStarted {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        tokens_before: u64,
    },
    /// A diagnostics/metrics tick (prefix stability, cache invalidation,
    /// call metrics, token usage) — logged, low wire priority.
    #[serde(rename = "metrics", rename_all = "camelCase")]
    Metrics {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        metric_type: String,
        data: JsonValue,
    },
    /// A swarm run started: one delegated child session under this log.
    #[serde(rename = "run/started", rename_all = "camelCase")]
    RunStarted {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        run_id: String,
        kind: String,
        label: String,
        child_session: String,
        tool_call_id: String,
        budgets: Budgets,
        depth: u32,
    },
    /// A swarm run settled — exactly one `run/settled` per `run/started`,
    /// whatever the ending. `verdict` is host-authored judgment, always
    /// present, including the explicit "It left no closing message." when
    /// salvage found nothing. Attributed to the host, never to the child.
    #[serde(rename = "run/settled", rename_all = "camelCase")]
    RunSettled {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        run_id: String,
        settlement: SettlementKind,
        verdict: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        salvage: Option<String>,
        turns: u64,
        tool_calls: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gated_denials: Option<Vec<String>>,
    },
    /// A swarm child asked its parent's user a question (the answer returns
    /// through `auth_id` into the child's next turn).
    #[serde(rename = "run/escalated", rename_all = "camelCase")]
    RunEscalated {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        run_id: String,
        question: String,
        auth_id: String,
    },
    /// A follow-up prompt was injected into an existing child session.
    #[serde(rename = "run/continued", rename_all = "camelCase")]
    RunContinued {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        run_id: String,
        follow_up: String,
    },
}

impl Event {
    /// The wire `type` of this event, read off its own serialization — the
    /// one definition of the row name is the enum itself.
    pub fn kind(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str().map(str::to_string)))
            .unwrap_or_default()
    }

    /// Whether the event proves the user interacted with the session — a
    /// user message in the log. Blank detection (claim reuse, sidebar
    /// hiding) keys on this.
    pub fn marks_interaction(&self) -> bool {
        matches!(
            self,
            Event::Message {
                message: crate::types::AgentMessage::User { .. },
                ..
            }
        )
    }
}

/// One log line: a dense sequence number, the commit time, and the event
/// itself flattened in beside them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub event: Event,
}

impl Envelope {
    pub fn new(seq: u64, event: Event) -> Self {
        Self {
            seq,
            ts: Utc::now(),
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_wire_carries_the_domain_type_names() {
        let envelope = Envelope {
            seq: 3,
            ts: DateTime::<Utc>::UNIX_EPOCH,
            event: Event::Title {
                id: "e-3".into(),
                parent_id: None,
                timestamp: DateTime::<Utc>::UNIX_EPOCH,
                title: "the panel question".into(),
            },
        };
        let wire = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            wire,
            r#"{"seq":3,"ts":"1970-01-01T00:00:00Z","type":"title","id":"e-3","parentId":null,"timestamp":"1970-01-01T00:00:00Z","title":"the panel question"}"#
        );
        let back: Envelope = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.seq, envelope.seq);
        assert_eq!(back.event.kind(), "title");
        assert!(matches!(
            back.event,
            Event::Title { ref title, .. } if title == "the panel question"
        ));
    }

    #[test]
    fn run_lifecycle_round_trips_with_settlement_kinds() {
        let started = Event::RunStarted {
            id: "e-0".into(),
            parent_id: None,
            timestamp: DateTime::<Utc>::UNIX_EPOCH,
            run_id: "sub-1".into(),
            kind: "Explore".into(),
            label: "survey the projection layer".into(),
            child_session: "sub-1".into(),
            tool_call_id: "call-7".into(),
            budgets: Budgets {
                timeout_ms: Some(240_000),
                idle_timeout_ms: None,
            },
            depth: 1,
        };
        let settled = Event::RunSettled {
            id: "e-1".into(),
            parent_id: None,
            timestamp: DateTime::<Utc>::UNIX_EPOCH,
            run_id: "sub-1".into(),
            settlement: SettlementKind::TimedOut,
            verdict: "exceeded its 240s budget; promoted, then settled".into(),
            salvage: Some("partial findings".into()),
            turns: 12,
            tool_calls: 34,
            gated_denials: Some(vec!["Bash".into(), "Bash".into()]),
        };
        for event in [&started, &settled] {
            let envelope = Envelope {
                seq: 1,
                ts: DateTime::<Utc>::UNIX_EPOCH,
                event: event.clone(),
            };
            let wire = serde_json::to_string(&envelope).unwrap();
            let back: Envelope = serde_json::from_str(&wire).unwrap();
            assert_eq!(back.event.kind(), event.kind());
        }
        let wire = serde_json::to_value(&settled).unwrap();
        assert_eq!(wire["type"], "run/settled");
        assert_eq!(wire["settlement"], "timed-out");
        assert_eq!(wire["gatedDenials"].as_array().map(Vec::len), Some(2));
        assert_eq!(wire["runId"], "sub-1");
    }

    #[test]
    fn a_user_message_marks_interaction_and_nothing_else_does() {
        let user = Event::Message {
            id: "e-9".into(),
            parent_id: None,
            timestamp: DateTime::<Utc>::UNIX_EPOCH,
            message: crate::types::AgentMessage::User {
                content: Vec::new(),
                timestamp: DateTime::<Utc>::UNIX_EPOCH,
                id: None,
            },
            origin: None,
        };
        assert!(user.marks_interaction());
        let other = Event::Title {
            id: "e-10".into(),
            parent_id: None,
            timestamp: DateTime::<Utc>::UNIX_EPOCH,
            title: "not interaction".into(),
        };
        assert!(!other.marks_interaction());
        let run = Event::RunSettled {
            id: "e-11".into(),
            parent_id: None,
            timestamp: DateTime::<Utc>::UNIX_EPOCH,
            run_id: "sub-1".into(),
            settlement: SettlementKind::Completed,
            verdict: "done".into(),
            salvage: None,
            turns: 1,
            tool_calls: 0,
            gated_denials: None,
        };
        assert!(!run.marks_interaction());
    }

    #[test]
    fn every_settlement_kind_keeps_its_own_wire_name() {
        for (kind, name) in [
            (SettlementKind::Completed, "completed"),
            (SettlementKind::TimedOut, "timed-out"),
            (SettlementKind::Stalled, "stalled"),
            (SettlementKind::Aborted, "aborted"),
            (SettlementKind::Error, "error"),
            (SettlementKind::Refusal, "refusal"),
            (SettlementKind::MaxTokens, "max-tokens"),
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(name.into())
            );
        }
    }

    #[test]
    fn unknown_event_types_are_refused_not_guessed() {
        let bad = r#"{"seq":1,"ts":"1970-01-01T00:00:00Z","type":"quantum/flip"}"#;
        assert!(serde_json::from_str::<Envelope>(bad).is_err());
    }

    /// A retired row name is refused like any other unknown one. The
    /// vocabulary has no compatibility reads: a journal naming a row this
    /// build does not write fails loudly instead of folding to a guess.
    #[test]
    fn a_retired_row_name_is_refused_like_an_unknown_one() {
        let bad = r#"{"seq":1,"ts":"1970-01-01T00:00:00Z","type":"session/disposed","id":"e-1","parentId":null,"timestamp":"1970-01-01T00:00:00Z"}"#;
        assert!(serde_json::from_str::<Envelope>(bad).is_err());
    }
}
