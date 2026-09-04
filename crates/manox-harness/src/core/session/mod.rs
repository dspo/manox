// Session storage and context building.
//
// A session is a tree of entries persisted as JSONL. Each entry has an id and
// parentId, forming a DAG walked leafward to reconstruct the conversation
// context. Variant `type` tags are snake_case and field names are camelCase,
// matching the TS Pi v3 on-disk schema exactly so real session files load.

pub mod jsonl;
pub mod repository;

use crate::types::{AgentMessage, Usage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// A single entry in the session tree.
///
/// Field names serialize as camelCase to match the TS Pi v3 schema. A `leaf`
/// entry records a cursor move to an older branch point: its `targetId` is the
/// entry the cursor now points at, and the leaf entry itself is never walked
/// (the cursor is `targetId`, not the leaf entry's own id).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionTreeEntry {
    #[serde(rename = "message", rename_all = "camelCase")]
    Message {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        message: AgentMessage,
        /// The RPC id this message was submitted under (§C.2 `originRpc`,
        /// dsh `source.rpcId`). The server pins the client's Submit
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
        // TS types `label` as `string | undefined` and omits it when unset;
        // skip-on-None keeps Rust output byte-identical to a TS-written entry.
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
    /// An assistant text delta (durable streaming chunk, dsh parity).
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
    /// The approval mode changed.
    #[serde(rename = "permission_mode_change", rename_all = "camelCase")]
    PermissionModeChange {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        mode: String,
    },
    /// Plan mode toggled.
    #[serde(rename = "plan_mode_change", rename_all = "camelCase")]
    PlanModeChange {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        enabled: bool,
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
}

/// Borrowed envelope fields shared by every [`SessionTreeEntry`] variant.
pub(crate) struct EntryEnvelope<'a> {
    pub(crate) id: &'a str,
    pub(crate) parent_id: Option<&'a str>,
    pub(crate) timestamp: DateTime<Utc>,
}

impl SessionTreeEntry {
    /// The envelope fields every variant carries. One macro-driven match so a
    /// new variant only ever adds one token here (the pre-v4 accessors were
    /// three parallel exhaustive matches).
    fn envelope(&self) -> EntryEnvelope<'_> {
        macro_rules! envelope_match {
            ($($variant:ident),* $(,)?) => {
                match self {
                    $(
                        SessionTreeEntry::$variant {
                            id, parent_id, timestamp, ..
                        } => EntryEnvelope {
                            id,
                            parent_id: parent_id.as_deref(),
                            timestamp: *timestamp,
                        },
                    )*
                }
            };
        }
        envelope_match!(
            Message,
            Compaction,
            ModelChange,
            ThinkingLevelChange,
            CwdChange,
            ActiveToolsChange,
            BranchSummary,
            Custom,
            CustomMessage,
            Label,
            SessionInfo,
            Leaf,
            UiNote,
            TurnStart,
            TurnFinish,
            Stop,
            Retry,
            ErrorEvent,
            AgentTextDelta,
            AgentThinkingDelta,
            ToolCall,
            ToolResult,
            ToolOutputChunk,
            SubagentChild,
            SubagentProgress,
            ProjectChange,
            PermissionModeChange,
            PlanModeChange,
            PlanUpdate,
            Goal,
            Title,
            BrowserSuites,
            BackgroundTask,
            Approval,
            PinnedArchived,
            CompactionStarted,
            Metrics,
        )
    }

    pub fn id(&self) -> &str {
        self.envelope().id
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.envelope().parent_id
    }

    pub fn timestamp(&self) -> DateTime<Utc> {
        self.envelope().timestamp
    }

    /// Build a typed entry from its wire kind + payload (§C.2). The payload
    /// object is merged over the envelope fields and the result must
    /// deserialize as exactly one typed variant — an unknown kind or a
    /// payload with wrong/missing fields fails loudly rather than falling
    /// back to `custom` (the typed vocabulary is the declared surface, L12).
    /// Envelope-key exclusivity (§C.1) is enforced by refusing payload keys
    /// `id`/`parentId`/`timestamp`/`type`/`seq`.
    pub fn from_kind_payload(
        kind: &str,
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        payload: JsonValue,
    ) -> Result<Self, anyhow::Error> {
        let payload = payload
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("journal payload for {kind} must be an object"))?;
        for reserved in ["id", "parentId", "timestamp", "type", "seq"] {
            if payload.contains_key(reserved) {
                anyhow::bail!(
                    "journal payload for {kind} uses reserved envelope key {reserved} (§C.1)"
                );
            }
        }
        if matches!(kind, "message" | "compaction") {
            anyhow::bail!(
                "kind {kind} owns a dedicated append path; append_typed refuses to bypass it"
            );
        }
        let mut value = serde_json::json!({
            "type": kind,
            "id": id,
            "parentId": parent_id,
            "timestamp": timestamp,
        });
        if let Some(obj) = value.as_object_mut() {
            for (key, field) in payload {
                obj.insert(key.clone(), field.clone());
            }
        }
        serde_json::from_value(value).map_err(|err| {
            anyhow::anyhow!(
                "journal payload for kind {kind} does not match the typed variant: {err}"
            )
        })
    }

    /// The leaf cursor after appending this entry: a `leaf` entry redirects to
    /// its `targetId`; every other entry's cursor is its own id.
    pub(crate) fn leaf_cursor_after(&self) -> Option<String> {
        match self {
            SessionTreeEntry::Leaf { target_id, .. } => target_id.clone(),
            _ => Some(self.id().to_string()),
        }
    }
}

/// Trait for session storage backends.
#[async_trait::async_trait]
pub trait SessionStorage: Send + Sync {
    /// Generate a new unique entry ID.
    async fn create_entry_id(&self) -> Result<String, anyhow::Error>;

    /// Append an entry to the session and synchronously advance the leaf
    /// cursor to it.
    ///
    /// The cursor after the append is the entry's own id, except for a `leaf`
    /// entry whose cursor is its `targetId`. Implementations must update their
    /// persisted cursor within this call so a later `get_leaf_id` reflects the
    /// append even if no further write follows — callers must not pair
    /// `append_entry` with a separate `set_leaf_id` for the same cursor move.
    async fn append_entry(&self, entry: &SessionTreeEntry) -> Result<(), anyhow::Error>;

    /// Get an entry by ID.
    async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, anyhow::Error>;

    /// Get the current leaf ID (cursor position).
    async fn get_leaf_id(&self) -> Result<Option<String>, anyhow::Error>;

    /// Move the cursor to an older branch point by appending a `leaf` entry.
    /// Implementations must validate that the target id exists before
    /// persisting; the appended entry's cursor (its `targetId`) becomes the
    /// new leaf.
    async fn set_leaf_id(&self, leaf_id: Option<&str>) -> Result<(), anyhow::Error>;

    /// Get a window of entries in append order. See [`SessionEntryCursor`].
    async fn get_entries(
        &self,
        cursor: SessionEntryCursor,
    ) -> Result<Vec<SessionTreeEntry>, anyhow::Error>;

    /// Every entry of one type, in append order.
    async fn find_entries(
        &self,
        entry_type: EntryType,
    ) -> Result<Vec<SessionTreeEntry>, anyhow::Error>;

    /// The label attached to an entry. Later labels supersede earlier ones for
    /// the same target, and a blank label reads as no label — that is how a
    /// label is cleared.
    async fn get_label(&self, id: &str) -> Result<Option<String>, anyhow::Error>;

    /// The session's name: the latest one set, blank reading as unnamed.
    async fn get_session_name(&self) -> Result<Option<String>, anyhow::Error>;

    /// What the session cost. See [`SessionStats`].
    async fn get_session_stats(&self) -> Result<SessionStats, anyhow::Error>;

    /// Walk from the leaf to the root, returning the full path in
    /// chronological order. Compaction boundaries do not stop the walk —
    /// callers decide how to project them. `None` yields an empty path. An
    /// explicit `leaf_id` unknown to storage is an error, as is a parent id
    /// with no entry: a truncated path would silently drop history.
    async fn get_path(&self, leaf_id: Option<&str>)
    -> Result<Vec<SessionTreeEntry>, anyhow::Error>;
}

/// A session wraps a SessionStorage with context-building logic.
pub struct Session<S: SessionStorage> {
    storage: S,
    /// Serializes parent-selection + append so concurrent appends never read
    /// the same leaf and fork sibling branches — the linearized per-session
    /// append queue of the TS storage (upstream 4488ad55c).
    append_lock: tokio::sync::Mutex<()>,
    /// The RPC id a client pinned to THIS turn's first user message (the
    /// echo-retirement contract, §F.2): the host sets it when Submit carries
    /// `origin_rpc`; the persistence middleware drains it on exactly that
    /// append. One-shot by construction.
    pending_user_origin: std::sync::Mutex<Option<String>>,
}

/// Authorship of a persisted compaction: whether a before-compact hook
/// supplied the summary (skipping the summarization model call), and the
/// structured payload it attached. The model path passes `from_hook: false`
/// with no `details`.
#[derive(Debug, Clone, Default)]
pub struct CompactionAuthorship {
    pub details: Option<JsonValue>,
    pub from_hook: bool,
}

impl<S: SessionStorage> Session<S> {
    pub fn new(storage: S) -> Self {
        Session {
            storage,
            append_lock: tokio::sync::Mutex::new(()),
            pending_user_origin: std::sync::Mutex::new(None),
        }
    }

    /// Pin the origin RPC id for this turn's first user message (§F.2).
    pub fn set_pending_user_origin(&self, origin: Option<String>) {
        *self.pending_user_origin.lock().unwrap() = origin;
    }

    /// Drain the pending origin (the persistence middleware's one-shot take
    /// on the user-message append).
    pub fn take_pending_user_origin(&self) -> Option<String> {
        self.pending_user_origin.lock().unwrap().take()
    }

    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Append a message entry and return the entry ID.
    pub async fn append_message(&self, message: AgentMessage) -> Result<String, anyhow::Error> {
        self.append_message_with_origin(message, None).await
    }

    /// Append a message entry carrying an optional `origin` (the RPC id it was
    /// submitted under, §C.2 `originRpc`) and return the entry ID. The server
    /// pins the client Submit's `origin_rpc` on the user message's journal
    /// entry so the client can retire its optimistic echo (§F.2). A `None`
    /// origin serializes to no `origin` key, byte-identical to the pre-T5b
    /// wire form.
    pub async fn append_message_with_origin(
        &self,
        message: AgentMessage,
        origin: Option<String>,
    ) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;

        let entry = SessionTreeEntry::Message {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            message,
            origin,
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Append a `model_change` entry and return the entry ID.
    pub async fn append_model_change(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;

        let entry = SessionTreeEntry::ModelChange {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Append a `cwd_change` entry and return the entry ID.
    ///
    /// The sticky cwd a path carries round-trips through these entries:
    /// restore projects the latest one as the session's effective working
    /// directory (the header cwd stays the launch directory forever).
    pub async fn append_cwd_change(&self, cwd: &str) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;

        let entry = SessionTreeEntry::CwdChange {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            cwd: cwd.to_string(),
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Append an `active_tools_change` entry and return the entry ID.
    pub async fn append_active_tools_change(
        &self,
        active_tool_names: &[String],
    ) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;

        let entry = SessionTreeEntry::ActiveToolsChange {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            active_tool_names: active_tool_names.to_vec(),
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Append a `thinking_level_change` entry and return the entry ID.
    ///
    /// The reasoning tier a path carries round-trips through these entries:
    /// restore projects the latest one onto the agent (`None` on the session
    /// reads as `"off"`).
    pub async fn append_thinking_level_change(&self, level: &str) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;

        let entry = SessionTreeEntry::ThinkingLevelChange {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            thinking_level: level.to_string(),
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Append a compaction entry and return the entry ID and timestamp.
    ///
    /// The leaf cursor moves to the entry, so later messages parent onto it.
    /// The retained tail persists with the boundary, making it a
    /// self-contained checkpoint; `first_kept_entry_id` stays as the tree-walk
    /// fallback for boundaries written without one. The returned timestamp is
    /// the boundary instant: the in-transcript summary message carries the
    /// same one, so a restored transcript matches the post-compaction one
    /// exactly.
    pub async fn append_compaction(
        &self,
        summary: &str,
        first_kept_entry_id: Option<String>,
        tokens_before: u64,
        usage: Option<Usage>,
        authorship: CompactionAuthorship,
        retained_tail: Option<Vec<AgentMessage>>,
    ) -> Result<(String, DateTime<Utc>), anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;
        let timestamp = Utc::now();

        let entry = SessionTreeEntry::Compaction {
            id: id.clone(),
            parent_id,
            timestamp,
            summary: summary.to_string(),
            first_kept_entry_id,
            tokens_before,
            retained_tail,
            usage,
            details: authorship.details,
            from_hook: Some(authorship.from_hook),
        };
        self.storage.append_entry(&entry).await?;
        Ok((id, timestamp))
    }

    /// Timestamp of the compaction bounding the current path, if any.
    ///
    /// The boundary is path-relative: only the latest compaction reachable
    /// from the current leaf counts, never another branch's.
    pub async fn latest_compaction_timestamp(
        &self,
    ) -> Result<Option<DateTime<Utc>>, anyhow::Error> {
        let entries = self.build_context_entries().await?;
        Ok(match entries.first() {
            Some(SessionTreeEntry::Compaction { timestamp, .. }) => Some(*timestamp),
            _ => None,
        })
    }

    /// Walk from the current leaf to the root, returning every entry on the
    /// active path in chronological order — all entry types, across however
    /// many compaction boundaries the path spans.
    pub async fn get_branch(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        let leaf_id = self.storage.get_leaf_id().await?;
        self.storage.get_path(leaf_id.as_deref()).await
    }

    /// Build the active, compaction-aware entry list.
    ///
    /// When the active path contains a compaction, the latest one heads the
    /// list, followed by the kept entries from its `first_kept_entry_id`
    /// onward (reconstructed from the tree, never from the boundary itself)
    /// and every entry after the boundary. Older summarized entries are
    /// omitted. Without a compaction this is the whole branch.
    pub async fn build_context_entries(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        let path = self.get_branch().await?;
        Ok(build_context_entries(path))
    }

    /// Build the session context: the messages a restored agent continues
    /// with, plus the settings the active path carries.
    pub async fn build_session_context(&self) -> Result<SessionContext, anyhow::Error> {
        let path = self.get_branch().await?;
        let (has_thinking_entry, thinking_level, model, active_tool_names, cwd) =
            context_settings(&path);
        let entries = build_context_entries(path);
        let mut messages = Vec::new();
        let mut message_entry_ids = Vec::new();
        for entry in &entries {
            let projected = session_entry_to_context_messages(entry);
            if !projected.is_empty() {
                message_entry_ids.push(Some(entry.id().to_string()));
                // Messages materialized inside the entry (a compaction's
                // retained tail) have no entry ids of their own.
                message_entry_ids.extend((1..projected.len()).map(|_| None));
                messages.extend(projected);
            }
        }
        Ok(SessionContext {
            messages,
            message_entry_ids,
            thinking_level,
            model,
            has_thinking_entry,
            active_tool_names,
            cwd,
        })
    }

    /// Append a `branch_summary` entry and return the entry ID.
    pub async fn append_branch_summary(
        &self,
        from_id: &str,
        summary: &str,
        read_files: &[String],
        modified_files: &[String],
        usage: Option<Usage>,
        from_hook: bool,
    ) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;
        let entry = SessionTreeEntry::BranchSummary {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            from_id: from_id.to_string(),
            summary: summary.to_string(),
            details: Some(serde_json::json!({
                "readFiles": read_files,
                "modifiedFiles": modified_files,
            })),
            usage,
            from_hook: Some(from_hook),
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Move the session cursor to an earlier entry, appending a `leaf` entry
    /// that records the branch point — the TS `moveTo`. `None` resets the
    /// cursor to the root.
    pub async fn move_to(&self, target_id: Option<&str>) -> Result<(), anyhow::Error> {
        self.storage.set_leaf_id(target_id).await
    }

    /// The current leaf (cursor) entry id.
    pub async fn leaf_id(&self) -> Result<Option<String>, anyhow::Error> {
        self.storage.get_leaf_id().await
    }

    /// Append a `custom` entry whose payload the harness does not interpret.
    pub async fn append_custom(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;
        let entry = SessionTreeEntry::Custom {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            custom_type: custom_type.to_string(),
            data,
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Append a typed v4 journal entry by its wire kind (§C.2). The payload
    /// object's keys must match the variant's camelCase field names;
    /// `id`/`parentId`/`timestamp` are assigned here so the chain stays
    /// single-writer. `message`/`compaction` are refused — they own richer
    /// append paths (`append_message`, the compaction flow) that must not be
    /// bypassed.
    pub async fn append_typed(
        &self,
        kind: &str,
        payload: JsonValue,
    ) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;
        let entry = SessionTreeEntry::from_kind_payload(kind, id, parent_id, Utc::now(), payload)?;
        self.storage.append_entry(&entry).await?;
        Ok(entry.id().to_string())
    }

    /// Append a `custom_message` entry whose payload the harness does not
    /// interpret; it joins the transcript like any message.
    pub async fn append_custom_message(
        &self,
        custom_type: &str,
        content: Vec<crate::types::ContentBlock>,
        details: Option<JsonValue>,
        display: bool,
    ) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;
        let entry = SessionTreeEntry::CustomMessage {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            custom_type: custom_type.to_string(),
            content,
            details,
            display,
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Attach a short human-readable label to an entry in the tree.
    pub async fn append_label(
        &self,
        target_id: &str,
        label: Option<String>,
    ) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;
        let entry = SessionTreeEntry::Label {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            target_id: target_id.to_string(),
            label,
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Set the session's display name via a `session_info` entry.
    pub async fn set_session_name(&self, name: &str) -> Result<String, anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;
        let entry = SessionTreeEntry::SessionInfo {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            name: Some(name.to_string()),
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// What this session cost. See [`SessionStats`].
    pub async fn stats(&self) -> Result<SessionStats, anyhow::Error> {
        self.storage.get_session_stats().await
    }

    /// The label attached to an entry, if any.
    pub async fn label(&self, id: &str) -> Result<Option<String>, anyhow::Error> {
        self.storage.get_label(id).await
    }

    /// The session's name, if one was set.
    pub async fn name(&self) -> Result<Option<String>, anyhow::Error> {
        self.storage.get_session_name().await
    }

    /// Every entry of one type, in append order.
    pub async fn find_entries(
        &self,
        entry_type: EntryType,
    ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        self.storage.find_entries(entry_type).await
    }

    /// A window of entries in append order. See [`SessionEntryCursor`].
    pub async fn page(
        &self,
        cursor: SessionEntryCursor,
    ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        self.storage.get_entries(cursor).await
    }
}

/// A window into the append-ordered entry list: everything after
/// `after_entry_seq`, capped at `limit`.
///
/// The default reads the whole list from the start. A cursor past the end
/// yields no entries rather than an error, so a caller polling for new entries
/// can hold a cursor across appends without special-casing the boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionEntryCursor {
    pub after_entry_seq: usize,
    pub limit: Option<usize>,
}

/// What a session cost, aggregated over every entry that carries usage.
///
/// `message_count` counts message entries whatever their role; the token and
/// cost figures come only from entries reporting a complete usage block —
/// assistant messages plus the compaction and branch-summary calls, which are
/// model calls the session paid for too.
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub message_count: usize,
    pub cached_tokens: u64,
    pub uncached_tokens: u64,
    pub total_tokens: u64,
    pub cost_total: f64,
}

/// The projected session state an agent is restored from.
pub struct SessionContext {
    /// Messages projected from the compaction-aware entry list.
    pub messages: Vec<AgentMessage>,
    /// The entry that produced each message, parallel to `messages`. Drives
    /// `first_kept_entry_id` resolution when the next compaction cuts the
    /// restored transcript.
    pub message_entry_ids: Vec<Option<String>>,
    /// The reasoning tier for following turns; `None` when the path left it
    /// at (or never changed it from) `"off"`.
    pub thinking_level: Option<String>,
    /// The model the session was last driven with — from the latest
    /// `model_change` entry, or the latest assistant message's own identity.
    pub model: Option<SessionModelRef>,
    /// Whether the path carries an explicit thinking-level entry. Distinct
    /// from `thinking_level`'s `None` (which also means "off"): a persisted
    /// `"off"` is a real decision and must not be overridden by settings
    /// defaults on reopen.
    pub has_thinking_entry: bool,
    /// The active tool subset from the latest `active_tools_change` entry;
    /// `None` when the path never narrowed the mounted set.
    pub active_tool_names: Option<Vec<String>>,
    /// The effective working directory from the latest `cwd_change` entry;
    /// `None` when every tool call ran in the header (launch) directory.
    pub cwd: Option<String>,
}

/// A model reference carried by the session path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModelRef {
    pub provider: String,
    pub model_id: String,
}

/// The compaction-aware projection over an active path: latest compaction
/// first, then everything after the boundary. A boundary carrying a
/// `retained_tail` is a self-contained checkpoint — the kept segment is not
/// walked; without one, the kept segment is reconstructed from
/// `first_kept_entry_id`.
fn build_context_entries(path: Vec<SessionTreeEntry>) -> Vec<SessionTreeEntry> {
    let Some(compaction_idx) = path
        .iter()
        .rposition(|e| matches!(e, SessionTreeEntry::Compaction { .. }))
    else {
        return path;
    };
    let SessionTreeEntry::Compaction {
        first_kept_entry_id,
        retained_tail,
        ..
    } = &path[compaction_idx]
    else {
        unreachable!("rposition matched a compaction");
    };

    let mut context_entries = vec![path[compaction_idx].clone()];
    // A `first_kept_entry_id` absent from the path keeps nothing — the same
    // outcome an undefined id produces in a hand-edited TS session file.
    if retained_tail.is_none() {
        let mut found_first_kept = false;
        for entry in &path[..compaction_idx] {
            if Some(entry.id()) == first_kept_entry_id.as_deref() {
                found_first_kept = true;
            }
            if found_first_kept {
                context_entries.push(entry.clone());
            }
        }
    }
    context_entries.extend_from_slice(&path[compaction_idx + 1..]);
    context_entries
}

/// The settings the active path carries: the reasoning tier from the latest
/// `thinking_level_change`, the model from the latest `model_change` (an
/// assistant message's own identity is a fresher witness than an older
/// `model_change`, matching the TS projection), the active tool subset from
/// the latest `active_tools_change`, and the effective working directory from
/// the latest `cwd_change`.
#[allow(clippy::type_complexity)]
fn context_settings(
    path: &[SessionTreeEntry],
) -> (
    bool,
    Option<String>,
    Option<SessionModelRef>,
    Option<Vec<String>>,
    Option<String>,
) {
    let mut has_thinking_entry = false;
    let mut thinking_level = None;
    let mut model = None;
    let mut active_tool_names = None;
    let mut cwd = None;
    for entry in path {
        match entry {
            SessionTreeEntry::ThinkingLevelChange {
                thinking_level: l, ..
            } => {
                has_thinking_entry = true;
                thinking_level = (l != "off").then(|| l.clone());
            }
            SessionTreeEntry::CwdChange { cwd: c, .. } => {
                cwd = Some(c.clone());
            }
            SessionTreeEntry::ModelChange {
                provider, model_id, ..
            } => {
                model = Some(SessionModelRef {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                });
            }
            SessionTreeEntry::Message {
                message:
                    AgentMessage::Assistant {
                        provider, model: m, ..
                    },
                ..
            } => {
                model = Some(SessionModelRef {
                    provider: provider.clone(),
                    model_id: m.clone(),
                });
            }
            SessionTreeEntry::ActiveToolsChange {
                active_tool_names: names,
                ..
            } => {
                active_tool_names = Some(names.clone());
            }
            _ => {}
        }
    }
    (
        has_thinking_entry,
        thinking_level,
        model,
        active_tool_names,
        cwd,
    )
}

/// Project one session entry into context messages. Display/state entries
/// (model/thinking/tool changes, labels, custom data, the cursor) produce
/// nothing.
pub fn session_entry_to_context_messages(entry: &SessionTreeEntry) -> Vec<AgentMessage> {
    match entry {
        SessionTreeEntry::Message { message, .. } => vec![message.clone()],
        SessionTreeEntry::CustomMessage {
            custom_type,
            content,
            details,
            display,
            timestamp,
            ..
        } => vec![AgentMessage::Custom {
            custom_type: custom_type.clone(),
            content: content.clone(),
            details: details.clone(),
            display: *display,
            timestamp: *timestamp,
        }],
        SessionTreeEntry::BranchSummary {
            summary, timestamp, ..
        } if !summary.is_empty() => vec![branch_summary_message(summary, *timestamp)],
        SessionTreeEntry::Compaction {
            summary,
            timestamp,
            retained_tail,
            ..
        } => {
            // The summary carrier heads the projection; a persisted tail
            // follows it, exactly as it did in the post-compaction transcript.
            let mut messages = Vec::with_capacity(1 + retained_tail.as_ref().map_or(0, Vec::len));
            messages.push(compaction_summary_message(summary, *timestamp));
            if let Some(tail) = retained_tail {
                messages.extend(tail.iter().cloned());
            }
            messages
        }
        _ => Vec::new(),
    }
}

/// The in-transcript carrier for a compaction summary: a tagged user
/// message. Kept symmetric between compaction and restore so the summary
/// reads identically whether it was just written or rebuilt from storage.
pub fn compaction_summary_message(summary: &str, timestamp: DateTime<Utc>) -> AgentMessage {
    AgentMessage::User {
        content: vec![crate::types::ContentBlock::Text {
            text: format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"),
            signature: None,
        }],
        timestamp,
    }
}

/// The in-transcript carrier for a branch summary: a tagged user message.
pub fn branch_summary_message(summary: &str, timestamp: DateTime<Utc>) -> AgentMessage {
    AgentMessage::User {
        content: vec![crate::types::ContentBlock::Text {
            text: format!("{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"),
            signature: None,
        }],
        timestamp,
    }
}

/// Tag wrapping a compaction summary in the transcript.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Tag wrapping a branch summary in the transcript.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContentBlock;

    fn message(id: &str, parent: Option<&str>, text: &str) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.into(),
            parent_id: parent.map(Into::into),
            timestamp: Utc::now(),
            message: AgentMessage::user(text),
            origin: None,
        }
    }

    fn compaction(
        id: &str,
        parent: Option<&str>,
        first_kept_entry_id: Option<&str>,
    ) -> SessionTreeEntry {
        SessionTreeEntry::Compaction {
            id: id.into(),
            parent_id: parent.map(Into::into),
            timestamp: Utc::now(),
            summary: format!("summary-{id}"),
            first_kept_entry_id: first_kept_entry_id.map(Into::into),
            tokens_before: 0,
            retained_tail: None,
            usage: None,
            details: None,
            from_hook: None,
        }
    }

    fn ids(entries: &[SessionTreeEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.id()).collect()
    }

    #[test]
    fn context_entries_reconstruct_the_kept_segment_from_the_tree() {
        // m1 m2 m3 [comp keeps from m2] m4: the context is the boundary, the
        // kept entries m2..m3 walked out of the pre-boundary path, then m4.
        let path = vec![
            message("m1", None, "one"),
            message("m2", Some("m1"), "two"),
            message("m3", Some("m2"), "three"),
            compaction("c1", Some("m3"), Some("m2")),
            message("m4", Some("c1"), "four"),
        ];
        let entries = build_context_entries(path);
        assert_eq!(ids(&entries), ["c1", "m2", "m3", "m4"]);
    }

    #[test]
    fn context_entries_last_compaction_wins() {
        // m1 [c1 keeps m1] m2 [c2 keeps nothing-known] m3: only the latest
        // boundary heads the context; the older one is summarized away.
        let path = vec![
            message("m1", None, "one"),
            compaction("c1", Some("m1"), Some("m1")),
            message("m2", Some("c1"), "two"),
            compaction("c2", Some("m2"), Some("m1")),
            message("m3", Some("c2"), "three"),
        ];
        let entries = build_context_entries(path);
        // c2's first_kept (m1) precedes c2 on the path, so the scan finds it
        // and keeps m1, c1, m2 — everything from m1 up to the boundary.
        assert_eq!(ids(&entries), ["c2", "m1", "c1", "m2", "m3"]);
    }

    #[test]
    fn context_entries_unknown_first_kept_keeps_nothing() {
        // A first_kept_entry_id that never appears on the path keeps no
        // pre-boundary entry at all.
        let path = vec![
            message("m1", None, "one"),
            message("m2", Some("m1"), "two"),
            compaction("c1", Some("m2"), Some("ghost")),
            message("m3", Some("c1"), "three"),
        ];
        let entries = build_context_entries(path);
        assert_eq!(ids(&entries), ["c1", "m3"]);
    }

    #[test]
    fn context_settings_take_the_latest_witness() {
        let assistant = |id: &str, parent: &str| SessionTreeEntry::Message {
            id: id.into(),
            parent_id: Some(parent.into()),
            timestamp: Utc::now(),
            message: AgentMessage::Assistant {
                content: vec![],
                model: "claude-opus".into(),
                provider: "anthropic".into(),
                api: "anthropic".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: None,
                usage: Box::default(),
                error_message: None,
                timestamp: Utc::now(),
            },
            origin: None,
        };
        let path = vec![
            SessionTreeEntry::ModelChange {
                id: "mc".into(),
                parent_id: None,
                timestamp: Utc::now(),
                provider: "openai".into(),
                model_id: "gpt-5".into(),
            },
            SessionTreeEntry::ThinkingLevelChange {
                id: "t1".into(),
                parent_id: Some("mc".into()),
                timestamp: Utc::now(),
                thinking_level: "medium".into(),
            },
            assistant("a1", "t1"),
            SessionTreeEntry::ActiveToolsChange {
                id: "at".into(),
                parent_id: Some("a1".into()),
                timestamp: Utc::now(),
                active_tool_names: vec!["Read".into(), "Bash".into()],
            },
            SessionTreeEntry::CwdChange {
                id: "cc1".into(),
                parent_id: Some("at".into()),
                timestamp: Utc::now(),
                cwd: "/tmp/wt-early".into(),
            },
            SessionTreeEntry::ThinkingLevelChange {
                id: "t2".into(),
                parent_id: Some("cc1".into()),
                timestamp: Utc::now(),
                thinking_level: "off".into(),
            },
            SessionTreeEntry::CwdChange {
                id: "cc2".into(),
                parent_id: Some("t2".into()),
                timestamp: Utc::now(),
                cwd: "/tmp/wt-late".into(),
            },
        ];
        let (has_thinking_entry, thinking_level, model, active_tool_names, cwd) =
            context_settings(&path);
        assert!(
            has_thinking_entry,
            "an explicit thinking entry is witnessed"
        );
        // The trailing "off" change resets the tier to the provider default.
        assert_eq!(thinking_level, None);
        // An assistant message is a fresher model witness than an older
        // model_change.
        assert_eq!(
            model,
            Some(SessionModelRef {
                provider: "anthropic".into(),
                model_id: "claude-opus".into(),
            })
        );
        // The latest active_tools_change carries the narrowed subset.
        assert_eq!(
            active_tool_names,
            Some(vec!["Read".to_string(), "Bash".to_string()])
        );
        // The latest cwd_change wins — the sticky cwd the path ends at.
        assert_eq!(cwd, Some("/tmp/wt-late".to_string()));
    }

    #[test]
    fn cwd_change_round_trips_the_wire_form() {
        let entry = SessionTreeEntry::CwdChange {
            id: "cc".into(),
            parent_id: Some("m1".into()),
            timestamp: Utc::now(),
            cwd: "/private/tmp/manox--wt".into(),
        };
        let wire = serde_json::to_string(&entry).unwrap();
        // The wire tag and camelCase fields must match the TS Pi v3 shape.
        assert!(wire.contains(r#""type":"cwd_change""#), "{wire}");
        assert!(wire.contains(r#""parentId":"m1""#), "{wire}");
        let back: SessionTreeEntry = serde_json::from_str(&wire).unwrap();
        match back {
            SessionTreeEntry::CwdChange { cwd, parent_id, .. } => {
                assert_eq!(cwd, "/private/tmp/manox--wt");
                assert_eq!(parent_id.as_deref(), Some("m1"));
            }
            other => panic!("expected CwdChange, got {other:?}"),
        }
    }

    #[test]
    fn message_origin_is_skipped_when_none_and_pinned_when_some() {
        // Echo retirement (§F.2) hinges on the wire form: a message with no
        // origin must be byte-identical to the pre-T5b form (no `origin` key),
        // and one with an origin must carry `originRpc`'s wire name `origin`.
        let base = |origin: Option<String>| SessionTreeEntry::Message {
            id: "m1".into(),
            parent_id: None,
            timestamp: Utc::now(),
            message: AgentMessage::user("hi"),
            origin,
        };

        let none_wire = serde_json::to_string(&base(None)).unwrap();
        assert!(
            !none_wire.contains("origin"),
            "an absent origin must not leak a key onto disk: {none_wire}"
        );

        let some_wire = serde_json::to_string(&base(Some("rpc-7".into()))).unwrap();
        assert!(
            some_wire.contains(r#""origin":"rpc-7""#),
            "a pinned origin must serialize under the `origin` wire name: {some_wire}"
        );
        let back: SessionTreeEntry = serde_json::from_str(&some_wire).unwrap();
        match back {
            SessionTreeEntry::Message { origin, .. } => {
                assert_eq!(origin.as_deref(), Some("rpc-7"));
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn entry_projection_covers_every_variant() {
        let ts = Utc::now();
        let custom_message = SessionTreeEntry::CustomMessage {
            id: "cm".into(),
            parent_id: None,
            timestamp: ts,
            custom_type: "notice".into(),
            content: vec![ContentBlock::Text {
                text: "heads up".into(),
                signature: None,
            }],
            details: Some(serde_json::json!({"level": 1})),
            display: true,
        };
        let projected = session_entry_to_context_messages(&custom_message);
        assert_eq!(projected.len(), 1);
        match &projected[0] {
            AgentMessage::Custom {
                custom_type,
                display,
                details,
                ..
            } => {
                assert_eq!(custom_type, "notice");
                assert!(display);
                assert_eq!(details, &Some(serde_json::json!({"level": 1})));
            }
            other => panic!("expected Custom, got {other:?}"),
        }

        let branch_summary = SessionTreeEntry::BranchSummary {
            id: "bs".into(),
            parent_id: None,
            timestamp: ts,
            from_id: "x".into(),
            summary: "came back".into(),
            details: None,
            usage: None,
            from_hook: None,
        };
        let projected = session_entry_to_context_messages(&branch_summary);
        assert_eq!(projected.len(), 1);
        match &projected[0] {
            AgentMessage::User { content, .. } => match &content[0] {
                ContentBlock::Text { text, .. } => assert_eq!(
                    text,
                    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\ncame back</summary>"
                ),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }

        // An empty branch summary produces nothing.
        let empty_summary = SessionTreeEntry::BranchSummary {
            id: "bs2".into(),
            parent_id: None,
            timestamp: ts,
            from_id: "x".into(),
            summary: String::new(),
            details: None,
            usage: None,
            from_hook: None,
        };
        assert!(session_entry_to_context_messages(&empty_summary).is_empty());

        // Display/state entries never enter the context.
        for entry in [
            SessionTreeEntry::ModelChange {
                id: "mc".into(),
                parent_id: None,
                timestamp: ts,
                provider: "p".into(),
                model_id: "m".into(),
            },
            SessionTreeEntry::ThinkingLevelChange {
                id: "tl".into(),
                parent_id: None,
                timestamp: ts,
                thinking_level: "high".into(),
            },
            SessionTreeEntry::ActiveToolsChange {
                id: "at".into(),
                parent_id: None,
                timestamp: ts,
                active_tool_names: vec![],
            },
            SessionTreeEntry::Custom {
                id: "cu".into(),
                parent_id: None,
                timestamp: ts,
                custom_type: "state".into(),
                data: None,
            },
            SessionTreeEntry::Label {
                id: "la".into(),
                parent_id: None,
                timestamp: ts,
                target_id: "x".into(),
                label: None,
            },
            SessionTreeEntry::SessionInfo {
                id: "si".into(),
                parent_id: None,
                timestamp: ts,
                name: None,
            },
            SessionTreeEntry::Leaf {
                id: "le".into(),
                parent_id: None,
                timestamp: ts,
                target_id: None,
            },
        ] {
            assert!(
                session_entry_to_context_messages(&entry).is_empty(),
                "{entry:?} must not project into the context"
            );
        }
    }

    #[test]
    fn compaction_projection_uses_the_summary_tags() {
        let entry = compaction("c1", None, None);
        let projected = session_entry_to_context_messages(&entry);
        assert_eq!(projected.len(), 1);
        match &projected[0] {
            AgentMessage::User { content, timestamp } => {
                assert_eq!(*timestamp, entry.timestamp());
                match &content[0] {
                    ContentBlock::Text { text, .. } => assert_eq!(
                        text,
                        "The conversation history before this point was compacted into the following summary:\n\n<summary>\nsummary-c1\n</summary>"
                    ),
                    other => panic!("expected text, got {other:?}"),
                }
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn compaction_with_retained_tail_projects_summary_then_tail() {
        let mut entry = compaction("c1", None, Some("m2"));
        let SessionTreeEntry::Compaction { retained_tail, .. } = &mut entry else {
            unreachable!()
        };
        *retained_tail = Some(vec![
            AgentMessage::user("kept one"),
            AgentMessage::user("kept two"),
        ]);

        let projected = session_entry_to_context_messages(&entry);
        assert_eq!(projected.len(), 3);
        let AgentMessage::User { content, .. } = &projected[1] else {
            panic!("tail message must project verbatim")
        };
        assert!(matches!(&content[0], ContentBlock::Text { text, .. } if text == "kept one"));
    }

    #[test]
    fn context_entries_retained_tail_skips_the_kept_walk() {
        // m1 m2 [c1 keeps m2 AND carries a materialized tail] m3: the tail
        // makes c1 self-contained — m2 is not walked out of the tree even
        // though first_kept_entry_id names it.
        let mut boundary = compaction("c1", Some("m2"), Some("m2"));
        let SessionTreeEntry::Compaction { retained_tail, .. } = &mut boundary else {
            unreachable!()
        };
        *retained_tail = Some(vec![AgentMessage::user("kept")]);
        let path = vec![
            message("m1", None, "one"),
            message("m2", Some("m1"), "two"),
            boundary,
            message("m3", Some("c1"), "three"),
        ];
        let entries = build_context_entries(path);
        assert_eq!(ids(&entries), ["c1", "m3"]);
    }

    #[tokio::test]
    async fn append_custom_lands_in_context_order_at_its_append_position() {
        let storage = crate::harness::tests::MemStorage::new();
        let session = Session::new(storage);
        session
            .append_message(AgentMessage::user("one"))
            .await
            .unwrap();
        session
            .append_custom("manox_ui_note", Some(serde_json::json!({ "k": 1 })))
            .await
            .unwrap();
        session
            .append_message(AgentMessage::user("two"))
            .await
            .unwrap();
        let entries = session.build_context_entries().await.unwrap();
        assert!(matches!(&entries[0], SessionTreeEntry::Message { .. }));
        assert!(
            matches!(&entries[1], SessionTreeEntry::Custom { custom_type, .. } if custom_type == "manox_ui_note"),
            "the custom entry keeps its append position between the messages"
        );
        assert!(matches!(&entries[2], SessionTreeEntry::Message { .. }));
    }
}

/// The TS `SessionTreeEntry["type"]` discriminator used by branch queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    Message,
    Compaction,
    ModelChange,
    CwdChange,
    ThinkingLevelChange,
    ActiveToolsChange,
    BranchSummary,
    Custom,
    CustomMessage,
    Label,
    SessionInfo,
    Leaf,
    // ── v4 journal vocabulary (§C.2) ──────────────────────────────────────
    UiNote,
    TurnStart,
    TurnFinish,
    Stop,
    Retry,
    ErrorEvent,
    AgentTextDelta,
    AgentThinkingDelta,
    ToolCall,
    ToolResult,
    ToolOutputChunk,
    SubagentChild,
    SubagentProgress,
    ProjectChange,
    PermissionModeChange,
    PlanModeChange,
    PlanUpdate,
    Goal,
    Title,
    BrowserSuites,
    BackgroundTask,
    Approval,
    PinnedArchived,
    CompactionStarted,
    Metrics,
}

impl EntryType {
    /// The wire type tag.
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryType::Message => "message",
            EntryType::Compaction => "compaction",
            EntryType::ModelChange => "model_change",
            EntryType::CwdChange => "cwd_change",
            EntryType::ThinkingLevelChange => "thinking_level_change",
            EntryType::ActiveToolsChange => "active_tools_change",
            EntryType::BranchSummary => "branch_summary",
            EntryType::Custom => "custom",
            EntryType::CustomMessage => "custom_message",
            EntryType::Label => "label",
            EntryType::SessionInfo => "session_info",
            EntryType::Leaf => "leaf",
            EntryType::UiNote => "ui_note",
            EntryType::TurnStart => "turn_start",
            EntryType::TurnFinish => "turn_finish",
            EntryType::Stop => "stop",
            EntryType::Retry => "retry",
            EntryType::ErrorEvent => "error",
            EntryType::AgentTextDelta => "agent_text_delta",
            EntryType::AgentThinkingDelta => "agent_thinking_delta",
            EntryType::ToolCall => "tool_call",
            EntryType::ToolResult => "tool_result",
            EntryType::ToolOutputChunk => "tool_output_chunk",
            EntryType::SubagentChild => "subagent_child",
            EntryType::SubagentProgress => "subagent_progress",
            EntryType::ProjectChange => "project_change",
            EntryType::PermissionModeChange => "permission_mode_change",
            EntryType::PlanModeChange => "plan_mode_change",
            EntryType::PlanUpdate => "plan_update",
            EntryType::Goal => "goal",
            EntryType::Title => "title",
            EntryType::BrowserSuites => "browser_suites",
            EntryType::BackgroundTask => "background_task",
            EntryType::Approval => "approval",
            EntryType::PinnedArchived => "pinned_archived",
            EntryType::CompactionStarted => "compaction_started",
            EntryType::Metrics => "metrics",
        }
    }
}

/// Where a branch query starts traversing.
#[derive(Debug, Clone, Default)]
pub enum BranchStart {
    /// The active leaf (TS `start` unset).
    #[default]
    Leaf,
    /// Explicit `null`: no traversal, empty result.
    None,
    /// Start at this entry id.
    At(String),
}

/// The TS `SessionBranchQuery`: a bounded traversal of the active branch.
#[derive(Debug, Clone, Default)]
pub struct SessionBranchQuery {
    /// Entry where traversal starts; defaults to the active leaf. `None`
    /// (TS `null`) yields an empty result.
    pub start: BranchStart,
    /// Stop after the first entry of this type (inclusive).
    pub stop_at_type: Option<EntryType>,
    /// Stop after the entry with this id (inclusive).
    pub stop_at_id: Option<String>,
    /// Only return entries of this type.
    pub entry_type: Option<EntryType>,
    /// Only return `custom` entries with this custom type.
    pub custom_type: Option<String>,
    /// Traversal order; defaults to newest first (start toward root).
    pub oldest_first: bool,
    /// Maximum number of filtered entries to return (must be positive).
    pub limit: Option<usize>,
}

/// A branch-query failure carrying the TS error code.
#[derive(Debug, Clone, thiserror::Error)]
pub enum BranchQueryError {
    /// The traversal start entry does not exist.
    #[error("Entry {0} not found")]
    NotFound(String),
    /// A broken parent chain or a cycle corrupts the branch.
    #[error("{0}")]
    InvalidSession(String),
}

impl<S: SessionStorage> Session<S> {
    /// Find entries on the active branch under the given bounds — the
    /// upstream `findEntriesOnBranch`. Mirrors the TS semantics exactly:
    /// walk from `start` toward the root (newest first) or from the root
    /// toward `start` (oldest first), stop after `stopAtType` / `stopAtId`
    /// (inclusive, computed after the traversal), filter by type / custom
    /// type, then apply `limit`. A missing start is `not_found`; a broken
    /// parent chain or a cycle is `invalid_session`; `limit: 0` is refused;
    /// `start: null` yields an empty result.
    pub async fn find_entries_on_branch(
        &self,
        query: SessionBranchQuery,
    ) -> Result<Vec<SessionTreeEntry>, BranchQueryError> {
        if query.limit == Some(0) {
            return Err(BranchQueryError::InvalidSession(
                "limit must be a positive integer".into(),
            ));
        }
        let start_id = match query.start {
            BranchStart::None => return Ok(Vec::new()),
            BranchStart::Leaf => self
                .storage
                .get_leaf_id()
                .await
                .map_err(|e| BranchQueryError::InvalidSession(e.to_string()))?,
            BranchStart::At(id) => Some(id),
        };
        let Some(start_id) = start_id else {
            return Ok(Vec::new());
        };

        let mut path: Vec<SessionTreeEntry> = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut current = self
            .storage
            .get_entry(&start_id)
            .await
            .map_err(|e| BranchQueryError::InvalidSession(e.to_string()))?
            .ok_or_else(|| BranchQueryError::NotFound(start_id.clone()))?;
        loop {
            let id = current.id().to_string();
            if !visited.insert(id.clone()) {
                return Err(BranchQueryError::InvalidSession(format!(
                    "Session branch contains a cycle at {id}"
                )));
            }
            path.push(current.clone());
            if !query.oldest_first
                && (query.stop_at_id.as_deref() == Some(current.id())
                    || query.stop_at_type == Some(entry_kind(&current)))
            {
                break;
            }
            let Some(parent_id) = current.parent_id() else {
                break;
            };
            current = self
                .storage
                .get_entry(parent_id)
                .await
                .map_err(|e| BranchQueryError::InvalidSession(e.to_string()))?
                .ok_or_else(|| {
                    BranchQueryError::InvalidSession(format!("Entry {parent_id} not found"))
                })?;
        }

        let traversal = if query.oldest_first {
            path.reverse();
            path
        } else {
            path
        };
        let stop_index = if query.oldest_first {
            traversal.iter().position(|e| {
                query.stop_at_id.as_deref() == Some(e.id())
                    || query.stop_at_type == Some(entry_kind(e))
            })
        } else {
            None
        };
        let bounded = match stop_index {
            Some(i) => traversal[..=i].to_vec(),
            None => traversal,
        };
        let entries: Vec<SessionTreeEntry> = bounded
            .into_iter()
            .filter(|e| {
                query.entry_type.is_none_or(|t| t == entry_kind(e))
                    && query.custom_type.as_deref().is_none_or(|ct| {
                        matches!(e, SessionTreeEntry::Custom { custom_type, .. } if custom_type == ct)
                    })
            })
            .collect();
        Ok(match query.limit {
            Some(l) => entries.into_iter().take(l).collect(),
            None => entries,
        })
    }

    /// The first entry matching the query — the upstream `findEntryOnBranch`.
    pub async fn find_entry_on_branch(
        &self,
        query: SessionBranchQuery,
    ) -> Result<Option<SessionTreeEntry>, BranchQueryError> {
        let query = SessionBranchQuery {
            limit: Some(1),
            ..query
        };
        Ok(self.find_entries_on_branch(query).await?.into_iter().next())
    }
}

/// The entry-type tag of an entry. Sole discriminator, shared by the branch
/// queries and the type-filtered entry scan.
pub fn entry_kind(entry: &SessionTreeEntry) -> EntryType {
    match entry {
        SessionTreeEntry::Message { .. } => EntryType::Message,
        SessionTreeEntry::Compaction { .. } => EntryType::Compaction,
        SessionTreeEntry::ModelChange { .. } => EntryType::ModelChange,
        SessionTreeEntry::CwdChange { .. } => EntryType::CwdChange,
        SessionTreeEntry::ThinkingLevelChange { .. } => EntryType::ThinkingLevelChange,
        SessionTreeEntry::ActiveToolsChange { .. } => EntryType::ActiveToolsChange,
        SessionTreeEntry::BranchSummary { .. } => EntryType::BranchSummary,
        SessionTreeEntry::Custom { .. } => EntryType::Custom,
        SessionTreeEntry::CustomMessage { .. } => EntryType::CustomMessage,
        SessionTreeEntry::Label { .. } => EntryType::Label,
        SessionTreeEntry::SessionInfo { .. } => EntryType::SessionInfo,
        SessionTreeEntry::Leaf { .. } => EntryType::Leaf,
        SessionTreeEntry::UiNote { .. } => EntryType::UiNote,
        SessionTreeEntry::TurnStart { .. } => EntryType::TurnStart,
        SessionTreeEntry::TurnFinish { .. } => EntryType::TurnFinish,
        SessionTreeEntry::Stop { .. } => EntryType::Stop,
        SessionTreeEntry::Retry { .. } => EntryType::Retry,
        SessionTreeEntry::ErrorEvent { .. } => EntryType::ErrorEvent,
        SessionTreeEntry::AgentTextDelta { .. } => EntryType::AgentTextDelta,
        SessionTreeEntry::AgentThinkingDelta { .. } => EntryType::AgentThinkingDelta,
        SessionTreeEntry::ToolCall { .. } => EntryType::ToolCall,
        SessionTreeEntry::ToolResult { .. } => EntryType::ToolResult,
        SessionTreeEntry::ToolOutputChunk { .. } => EntryType::ToolOutputChunk,
        SessionTreeEntry::SubagentChild { .. } => EntryType::SubagentChild,
        SessionTreeEntry::SubagentProgress { .. } => EntryType::SubagentProgress,
        SessionTreeEntry::ProjectChange { .. } => EntryType::ProjectChange,
        SessionTreeEntry::PermissionModeChange { .. } => EntryType::PermissionModeChange,
        SessionTreeEntry::PlanModeChange { .. } => EntryType::PlanModeChange,
        SessionTreeEntry::PlanUpdate { .. } => EntryType::PlanUpdate,
        SessionTreeEntry::Goal { .. } => EntryType::Goal,
        SessionTreeEntry::Title { .. } => EntryType::Title,
        SessionTreeEntry::BrowserSuites { .. } => EntryType::BrowserSuites,
        SessionTreeEntry::BackgroundTask { .. } => EntryType::BackgroundTask,
        SessionTreeEntry::Approval { .. } => EntryType::Approval,
        SessionTreeEntry::PinnedArchived { .. } => EntryType::PinnedArchived,
        SessionTreeEntry::CompactionStarted { .. } => EntryType::CompactionStarted,
        SessionTreeEntry::Metrics { .. } => EntryType::Metrics,
    }
}

#[cfg(test)]
mod branch_query_tests {
    use super::*;
    use crate::types::ContentBlock;

    /// The upstream bounded branch-query semantics, ported from
    /// `branch-query.test.ts`: traversal bounds, order, filtering, limits,
    /// and error codes.
    #[tokio::test]
    async fn find_entries_on_branch_matches_upstream_semantics() {
        use crate::types::AgentMessage as M;
        fn assistant(text: &str) -> M {
            M::Assistant {
                content: vec![ContentBlock::Text {
                    text: text.into(),
                    signature: None,
                }],
                model: "test".into(),
                provider: "test".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(crate::types::StopReason::Stop),
                usage: Default::default(),
                error_message: None,
                timestamp: Utc::now(),
            }
        }
        let storage = crate::harness::tests::MemStorage::new();
        let session = Session::new(storage);
        let root = session.append_message(M::user("root")).await.unwrap();
        let custom = session
            .append_custom("note", Some(serde_json::json!({"value": 1})))
            .await
            .unwrap();
        let child = session.append_message(assistant("child")).await.unwrap();
        let (compaction, _) = session
            .append_compaction(
                "summary",
                Some(child.clone()),
                100,
                None,
                CompactionAuthorship {
                    details: None,
                    from_hook: false,
                },
                None,
            )
            .await
            .unwrap();
        let recent_custom = session
            .append_custom("note", Some(serde_json::json!({"value": 2})))
            .await
            .unwrap();
        let tail = session.append_message(M::user("tail")).await.unwrap();
        session.move_to(Some(&root)).await.unwrap();
        let sibling = session.append_message(M::user("sibling")).await.unwrap();

        let ids = |entries: Vec<SessionTreeEntry>| {
            entries
                .into_iter()
                .map(|e| e.id().to_string())
                .collect::<Vec<_>>()
        };
        let q = |start, oldest_first: bool| SessionBranchQuery {
            start,
            oldest_first,
            ..Default::default()
        };

        // Default (newest first) from the active leaf.
        assert_eq!(
            ids(session
                .find_entries_on_branch(SessionBranchQuery::default())
                .await
                .unwrap()),
            vec![sibling.clone(), root.clone()]
        );
        // Explicit null start: empty.
        assert!(
            session
                .find_entries_on_branch(q(BranchStart::None, false))
                .await
                .unwrap()
                .is_empty()
        );
        // Oldest first walks root -> start.
        assert_eq!(
            ids(session
                .find_entries_on_branch(q(BranchStart::At(tail.clone()), true))
                .await
                .unwrap()),
            vec![
                root.clone(),
                custom.clone(),
                child.clone(),
                compaction.clone(),
                recent_custom.clone(),
                tail.clone()
            ]
        );
        // stopAtType inclusive on the newest-first walk.
        assert_eq!(
            ids(session
                .find_entries_on_branch(SessionBranchQuery {
                    start: BranchStart::At(tail.clone()),
                    stop_at_type: Some(EntryType::Compaction),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![tail.clone(), recent_custom.clone(), compaction.clone()]
        );
        // stopAtType with a type filter drops the stop entry itself.
        assert_eq!(
            ids(session
                .find_entries_on_branch(SessionBranchQuery {
                    start: BranchStart::At(tail.clone()),
                    stop_at_type: Some(EntryType::Compaction),
                    entry_type: Some(EntryType::Message),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![tail.clone()]
        );
        // stopAtId on the oldest-first walk bounds from the root.
        assert_eq!(
            ids(session
                .find_entries_on_branch(SessionBranchQuery {
                    start: BranchStart::At(tail.clone()),
                    stop_at_id: Some(child.clone()),
                    oldest_first: true,
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![root.clone(), custom.clone(), child.clone()]
        );
        // stopAtType "custom" stops at the custom entry (newest first).
        assert_eq!(
            ids(session
                .find_entries_on_branch(SessionBranchQuery {
                    start: BranchStart::At(tail.clone()),
                    stop_at_type: Some(EntryType::Custom),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![tail.clone(), recent_custom.clone()]
        );
        // stopAtType "custom" oldest first stops at the earliest custom.
        assert_eq!(
            ids(session
                .find_entries_on_branch(SessionBranchQuery {
                    start: BranchStart::At(tail.clone()),
                    stop_at_type: Some(EntryType::Custom),
                    oldest_first: true,
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![root.clone(), custom.clone()]
        );
        // Type filter over the whole walk, oldest first.
        assert_eq!(
            ids(session
                .find_entries_on_branch(SessionBranchQuery {
                    start: BranchStart::At(tail.clone()),
                    entry_type: Some(EntryType::Message),
                    oldest_first: true,
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![root.clone(), child.clone(), tail.clone()]
        );
        // customType filters the custom entries.
        assert_eq!(
            ids(session
                .find_entries_on_branch(SessionBranchQuery {
                    start: BranchStart::At(tail.clone()),
                    custom_type: Some("note".into()),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![recent_custom.clone(), custom.clone()]
        );
        // limit applies after ordering: newest first keeps the start.
        assert_eq!(
            ids(session
                .find_entries_on_branch(SessionBranchQuery {
                    start: BranchStart::At(tail.clone()),
                    limit: Some(1),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![tail.clone()]
        );
        // limit on the oldest-first walk keeps the root.
        assert_eq!(
            ids(session
                .find_entries_on_branch(SessionBranchQuery {
                    start: BranchStart::At(tail.clone()),
                    entry_type: Some(EntryType::Message),
                    oldest_first: true,
                    limit: Some(1),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![root.clone()]
        );
        // findEntryOnBranch returns the first match.
        let found = session
            .find_entry_on_branch(SessionBranchQuery {
                start: BranchStart::At(tail.clone()),
                entry_type: Some(EntryType::Compaction),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id(), compaction);
        // Missing start: not_found.
        let err = session
            .find_entries_on_branch(q(BranchStart::At("missing".into()), false))
            .await
            .unwrap_err();
        assert!(matches!(err, BranchQueryError::NotFound(_)), "{err}");
        // limit 0 is refused.
        let err = session
            .find_entries_on_branch(SessionBranchQuery {
                start: BranchStart::At(tail.clone()),
                limit: Some(0),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
    }

    /// A broken parent chain and a cycle surface as invalid_session.
    #[tokio::test]
    async fn find_entries_on_branch_detects_cycles_and_broken_parents() {
        let storage = crate::harness::tests::MemStorage::new();
        let session = Session::new(storage);
        session
            .storage()
            .append_entry(&SessionTreeEntry::Message {
                id: "orphan".into(),
                parent_id: Some("missing-parent".into()),
                timestamp: Utc::now(),
                message: AgentMessage::user("orphan"),
                origin: None,
            })
            .await
            .unwrap();
        // stopAtId on the start itself still resolves (no parent walk).
        let entries = session
            .find_entries_on_branch(SessionBranchQuery {
                start: BranchStart::At("orphan".into()),
                stop_at_id: Some("orphan".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        // Without a stop, the broken parent errors as invalid_session.
        let err = session
            .find_entries_on_branch(q_orphan())
            .await
            .unwrap_err();
        assert!(matches!(err, BranchQueryError::InvalidSession(_)), "{err}");

        session
            .storage()
            .append_entry(&SessionTreeEntry::Message {
                id: "cycle-a".into(),
                parent_id: Some("cycle-b".into()),
                timestamp: Utc::now(),
                message: AgentMessage::user("a"),
                origin: None,
            })
            .await
            .unwrap();
        session
            .storage()
            .append_entry(&SessionTreeEntry::Message {
                id: "cycle-b".into(),
                parent_id: Some("cycle-a".into()),
                timestamp: Utc::now(),
                message: AgentMessage::user("b"),
                origin: None,
            })
            .await
            .unwrap();
        let err = session
            .find_entries_on_branch(SessionBranchQuery {
                start: BranchStart::At("cycle-b".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
    }

    fn q_orphan() -> SessionBranchQuery {
        SessionBranchQuery {
            start: BranchStart::At("orphan".into()),
            ..Default::default()
        }
    }
}
