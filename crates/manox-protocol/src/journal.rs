//! Journal wire vocabulary — the client-facing §C.2 entry set.
//!
//! Declaring surface: `JOURNAL_ENTRIES` (see [`crate::surface`]).
//! [`JournalWireEvent`] is the wire form of the kernel `JournalEntry` enum
//! (architecture doc §C.2): every observable state change of a thread is
//! carried by one of these entries (L3), stamped with a chain-dense `seq` at
//! the single append point (L4). The frames [`crate::stream`] carry are
//! `JournalWireEntry` = `{seq, id, parentId, timestamp, event}` (§C.1);
//! `StreamFrame::Entry` transports the `seq` + `event` pair.
//!
//! serde shape: internally tagged by `type` (camelCase), struct variants with
//! camelCase payload fields. `unknown-variant-tolerant` on the read side is a
//! client rule (L12: drop + log, never disconnect); the guards in
//! `bindings/guards.ts` implement it for TypeScript consumers.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Opaque handle of one server↔client stream (`StreamOpen` / `StreamItem` /
/// `StreamEnd` all carry it). Minted by the client; unique per connection.
///
/// Declaring surface: FRAMES (§D.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct StreamId(pub String);

impl StreamId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Canonical model reference on the wire: `{provider_registration}/{model_id}`
/// (e.g. `DeepSeek-anthropic/deepseek-chat`).
///
/// Per L8 the wire type never carries a bare model id: every model field in
/// this crate uses `ModelRef`. Resolution / compatibility for bare ids is a
/// server-only concern, converged in the single entry point
/// `resolve_model_ref` (`manox-harness::model_ref`); clients must never parse
/// domain identity beyond display splitting (L6).
///
/// Declaring surface: shared identity type of the JOURNAL_ENTRIES /
/// PROJECTION_KEYS / frame payloads (§D, L8).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct ModelRef(pub String);

impl ModelRef {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Per-request token usage carried by the assistant `message` entry (§C.2
/// transcript group). `anyhow`-style payloads are flattened to plain numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub struct UsagePayload {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

/// Thread header — the journal file's line 0 shape (§C.1), echoed into
/// [`crate::stream::SessionSnapshot`] so a snapshot is self-describing.
///
/// Declaring surface: frame payload of `StreamFrame::Snapshot` (§D.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub struct ThreadHeader {
    /// Thread / session id.
    pub id: String,
    /// Working directory the thread was created in.
    pub cwd: String,
    /// Leader session id for a team worker thread; absent for top-level.
    pub parent_session: Option<String>,
    /// Free-form persisted metadata (labels, plugin blobs).
    pub metadata: Option<serde_json::Value>,
    /// ISO-8601 creation timestamp (journal file shape).
    pub created_at: String,
}

/// One journal wire event: the §C.2 entry vocabulary, verbatim. Variant
/// payload fields match the §C.2 table; the row group is in each variant's
/// doc comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum JournalWireEvent {
    // ── transcript ──────────────────────────────────────────────────
    /// A conversation message (user / assistant / tool). Assistant messages
    /// carry `usage`; user messages may carry `originRpc` so the client
    /// retires its optimistic echo (L7 receipt + echo retirement).
    Message {
        /// `"user" | "assistant" | "tool"` — the persisted role vocabulary.
        role: String,
        /// Content blocks in the kernel's storage shape (wire-opaque).
        content: Vec<serde_json::Value>,
        /// Assistant per-request token usage; absent on other roles.
        usage: Option<UsagePayload>,
        /// RPC id of the `Submit`/`Steer` call that created this message,
        /// when a client echo must be retired by it.
        origin_rpc: Option<String>,
    },
    /// A durable UI annotation card (error / notice / plan-review). Was the
    /// `AppendUiNote` client note; now a replayable journal entry.
    UiNote {
        kind: String,
        data: serde_json::Value,
    },
    // ── lifecycle ───────────────────────────────────────────────────
    /// A model turn started (drives the `running` projection's true edge).
    TurnStart,
    /// A model turn finished. `cancelled` / `failed` classify the exit;
    /// `strandedSteerIds` lists steer messages that never injected.
    TurnFinish {
        cancelled: bool,
        failed: bool,
        stranded_steer_ids: Vec<String>,
    },
    /// The loop stopped advancing with an optional human reason.
    Stop { reason: Option<String> },
    /// A provider retry was scheduled.
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
        reason: String,
    },
    /// A terminal error (`anyhow::Error` flattened to `{ message }`, §C.2).
    Error { message: String },
    // ── streaming delta ─────────────────────────────────────────────
    /// An assistant text delta (chunked, durable — dsh parity).
    AgentTextDelta { s: String },
    /// An assistant thinking delta.
    AgentThinkingDelta { s: String },
    /// A tool call announced / updated. `status` is the vocabulary string
    /// (`"pending" | "running" | "done" | "error"`). The tool-call handle is
    /// `callId` — the envelope's `id` belongs to the entry uuid alone
    /// (flatten collision rule, §C.1).
    ToolCall {
        call_id: String,
        name: String,
        title: String,
        status: String,
        input: serde_json::Value,
    },
    /// A tool result settled (`callId` per the §C.1 collision rule).
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    /// A streaming chunk of a tool's stdout/stderr (`callId`, §C.1).
    ToolOutputChunk { call_id: String, chunk: String },
    /// An event surfaced by a subagent child session (`agentId`, §C.1).
    SubagentChild {
        agent_id: String,
        event: serde_json::Value,
    },
    /// Subagent progress tick (recorded on ≥500ms spacing or status change;
    /// `agentId`, §C.1).
    SubagentProgress {
        agent_id: String,
        agent_type: String,
        tool_uses: u32,
        latest_activity: Option<String>,
        status: String,
    },
    // ── state change ────────────────────────────────────────────────
    /// Model switched; `from`/`to` are canonical [`ModelRef`] (L8).
    ModelChange {
        from: Option<ModelRef>,
        to: ModelRef,
    },
    /// Effective working directory moved.
    CwdChange { path: String },
    /// Project binding changed (`None` = unbound).
    ProjectChange { path: Option<String> },
    /// Permission mode changed (vocabulary string).
    PermissionModeChange { mode: String },
    /// Reasoning effort changed (vocabulary string).
    ReasoningEffortChange { effort: String },
    /// Plan mode toggled.
    PlanModeChange { enabled: bool },
    /// The plan document updated (kernel snapshot shape, wire-opaque).
    PlanUpdate { snapshot: serde_json::Value },
    /// Goal set / cleared (`None` = cleared).
    Goal { goal: Option<serde_json::Value> },
    /// Thread title changed.
    Title { title: String },
    /// Active browser suites changed.
    BrowserSuites { suites: Vec<String> },
    /// Background-task registry snapshot updated.
    BackgroundTask { snapshot: serde_json::Value },
    /// Approval request / decision, dual-state (§C.2): the fold source of
    /// the `pending_auth` projection. `kind` is `"request"` | `"decision"`.
    Approval {
        kind: String,
        auth_id: String,
        tool_name: Option<String>,
        tool_call_id: Option<String>,
        verdict: Option<String>,
        reason: Option<String>,
    },
    /// Pinned / archived flags changed.
    PinnedArchived { pinned: bool, archived: bool },
    /// Compaction of older history completed (spinner state is carried by
    /// the independent `CompactionStarted` entry, §C.2 note).
    Compaction {
        summary: String,
        messages_compacted: u32,
        tokens_before: u64,
        retained_tail: Vec<serde_json::Value>,
        first_kept_entry_id: Option<String>,
    },
    /// Compaction of older history started (UI spinner edge).
    CompactionStarted { tokens_before: u64 },
    // ── compression / tree ──────────────────────────────────────────
    /// Summary entry produced when a branch point is collapsed.
    BranchSummary { text: String },
    /// A label was attached to an entry.
    Label { label: String },
    /// Session-info annotation row.
    SessionInfo { data: serde_json::Value },
    /// Leaf redirect (fork / merged follow-up; the `leaf.targetId` cursor
    /// semantics of §C.1). Carries only the target entry id.
    Leaf { target_id: String },
    // ── metrics ─────────────────────────────────────────────────────
    /// Diagnostic metric (low-priority group, §C.2):
    /// `kind` = `prefix_stability | cache_invalidation | side_call |
    /// main_call | token_usage`; `data` = kind-specific payload.
    Metrics {
        kind: String,
        data: serde_json::Value,
    },
}

/// One journal entry line as it travels the wire (§C.1 entry envelope):
/// chain-dense `seq` + identity + timestamp + the [`JournalWireEvent`].
/// `StreamFrame::Entry { seq, event }` carries the same pair inside the
/// frame tag (§D.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub struct JournalWireEntry {
    /// Chain depth, dense 0-based, stamped at the single append point (L4).
    pub seq: u64,
    /// Entry uuid.
    pub id: String,
    /// Parent entry uuid (chain edge).
    pub parent_id: Option<String>,
    /// ISO-8601 append timestamp (journal file shape).
    pub timestamp: String,
    /// The event vocabulary row.
    #[serde(flatten)]
    pub event: JournalWireEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> JournalWireEvent {
        JournalWireEvent::Message {
            role: "assistant".into(),
            content: vec![serde_json::json!({"text": "hi"})],
            usage: Some(UsagePayload {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4,
                reasoning: 5,
            }),
            origin_rpc: None,
        }
    }

    #[test]
    fn journal_entry_flattens_event_tag_into_line_shape() {
        let entry = JournalWireEntry {
            seq: 7,
            id: "e-1".into(),
            parent_id: Some("e-0".into()),
            timestamp: "2026-09-04T00:00:00Z".into(),
            event: sample(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["seq"], 7);
        assert_eq!(json["id"], "e-1");
        assert_eq!(json["parentId"], "e-0");
        // Internally-tagged payload flattened: type/role/content inline.
        assert_eq!(json["type"], "message");
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["usage"]["cacheRead"], 3);
        let back: JournalWireEntry = serde_json::from_value(json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn event_type_tag_is_camel_case() {
        let json =
            serde_json::to_value(JournalWireEvent::AgentTextDelta { s: "x".into() }).unwrap();
        assert_eq!(json["type"], "agentTextDelta");
        let json = serde_json::to_value(JournalWireEvent::ToolOutputChunk {
            call_id: "t".into(),
            chunk: "c".into(),
        })
        .unwrap();
        assert_eq!(json["type"], "toolOutputChunk");
        assert_eq!(json["chunk"], "c");
    }

    #[test]
    fn model_change_uses_canonical_model_ref() {
        let ev = JournalWireEvent::ModelChange {
            from: Some(ModelRef::new("anthropic-main/claude-sonnet-4")),
            to: ModelRef::new("DeepSeek-anthropic/deepseek-chat"),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["to"], "DeepSeek-anthropic/deepseek-chat");
        let back: JournalWireEvent = serde_json::from_value(json).unwrap();
        assert_eq!(ev, back);
    }
}
