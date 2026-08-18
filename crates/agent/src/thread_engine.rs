//! The harness-engine contract the `Thread` facade delegates to.
//!
//! The interface shape follows the pi `AgentSession` capabilities — run,
//! steer, abort, model/thinking switching, session switching — because pi is
//! the first-class harness; the manox implementation adapts to that shape
//! where the two disagree (manox yields). The trait is deliberately free of
//! gpui: a backend either owns its own async runtime (pi actor) or drives the
//! facade through injected callbacks, so the facade's `Context` never leaks
//! into the contract.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::db::ThreadSummary;
use crate::language_model::TokenUsage;
use crate::message::Message;
use crate::permission::{PendingAuthMeta, ToolAuthorizationResponse};
use crate::thread::ApprovalMode;
use pi::types::Model as PiModel;

/// Commands a facade can issue to its harness backend, plus the backend's
/// authoritative state the facade mirrors after a settled run.
pub trait ThreadEngine: Send + Sync {
    /// Whether a turn is currently in flight.
    fn is_running(&self) -> bool;

    /// The authoritative transcript after the latest settled run. The facade
    /// replaces its cached history with this on every settle.
    fn history(&self) -> Vec<Message>;

    /// Token usage keyed by user-message id, as the env card renders it.
    fn request_token_usage(&self) -> HashMap<String, TokenUsage>;

    /// The thread-wide cumulative usage across all settled turns.
    fn cumulative_token_usage(&self) -> TokenUsage {
        TokenUsage::default()
    }

    /// Per-model cumulative usage keyed by model id.
    fn per_model_token_usage(&self) -> HashMap<String, TokenUsage> {
        HashMap::new()
    }

    /// Cumulative USD cost of the thread (rate-card pricing at the wire
    /// boundary); backends without pricing report 0.
    fn cumulative_cost(&self) -> f64 {
        0.0
    }

    /// Per-model cumulative USD cost keyed like `per_model_token_usage`.
    fn per_model_cost(&self) -> HashMap<String, f64> {
        HashMap::new()
    }

    /// The model the backend currently runs, if any.
    fn model(&self) -> Option<PiModel>;

    /// Start a turn with the given user text and attached images (base64
    /// blocks per the kernel's `ContentBlock::Image`). Events flow back
    /// through the engine's event channel (spawned with the engine);
    /// settlement lands in `history`.
    fn run(&self, prompt: String, images: Vec<pi::types::ContentBlock>);

    /// Inject a steer (text + optional image attachments) into the running
    /// turn. Returns the steer id, which `cancel_steer` accepts to retract it
    /// before the loop drains it.
    fn steer(&self, text: String, images: Vec<pi::types::ContentBlock>) -> String;

    /// Retract a queued steer by id. False when it already drained.
    fn cancel_steer(&self, id: &str) -> bool;

    /// Abort the running turn.
    fn abort(&self);

    /// Hot-swap the model for the next provider request.
    fn set_model(&self, model: PiModel);

    /// Map the reasoning effort onto the backend's thinking level.
    fn set_thinking_level(&self, level: Option<String>);

    /// Re-point the backend at an existing session file.
    fn open_session(&self, path: PathBuf);

    /// Create a fresh session in the given directory.
    fn new_session(&self, cwd: PathBuf, project: Option<PathBuf>);

    /// The session file the backend currently drives, if any.
    fn active_session_path(&self) -> Option<PathBuf>;

    /// The sessions the backend can list (sidebar source), newest first.
    fn session_list(&self) -> Vec<ThreadSummary>;

    /// Ask the backend to wind down (close its session, stop its runtime).
    /// Default is a no-op for backends that need no explicit shutdown.
    fn shutdown(&self) {}

    /// Switch the approval policy gating the backend's tool executions.
    /// Backends without an approval gate ignore this.
    fn set_approval_mode(&self, _mode: ApprovalMode) {}

    /// Toggle plan mode (persisted sidecar + hooks + instructions).
    fn set_plan_mode(&self, _enabled: bool) {}

    /// Persist whether a plan review card is pending (restore re-surfaces it).
    fn set_plan_review_pending(&self, _pending: bool) {}

    /// Persist the latest `UpdatePlan` snapshot so the rail's plan survives
    /// compaction (the transcript's plan tool calls are summarized away).
    /// `None` clears it. Backends without sidecar persistence no-op.
    fn persist_plan_snapshot(&self, _snapshot: Option<serde_json::Value>) {}

    /// Mark an approved plan as the active execution title source and wake
    /// the private Title agent immediately.
    fn start_plan_execution(&self, _plan_file: String) {}

    /// Wake the private Title agent after a user-created/replaced Goal starts.
    fn goal_started(&self) {}

    /// Execute an approved plan: optional compaction toward the plan file,
    /// then the execution seed turn.
    fn approve_plan(
        &self,
        _compact: bool,
        _compact_instructions: Option<String>,
        _seed_text: String,
    ) {
    }

    /// Run a manual context-compaction pass (`/compact`). Backends without
    /// kernel compaction ignore this; backends that require an idle
    /// transcript no-op while a turn is in flight.
    fn compact(&self, _custom_instructions: Option<String>) {}

    /// Deliver the user's verdict for a pending tool-call authorization.
    /// Unknown ids are silently ignored (the call already settled).
    fn respond_tool_authorization(&self, _id: &str, _response: ToolAuthorizationResponse) {}

    /// Pending authorizations with their card metadata, so the workspace can
    /// re-surface a card after switching back to a parked thread.
    fn pending_auth_entries(&self) -> Vec<(String, PendingAuthMeta)> {
        Vec::new()
    }
}

/// Notices the backend sends back to the facade's gpui drainer.
pub enum BackendNotice {
    /// A run event already adapted into a `ThreadEvent`.
    Event(Box<crate::thread::ThreadEvent>),
    /// The session finished building/restoring.
    Ready {
        restored: bool,
        /// The model the actor resolved for this thread (registration may
        /// have landed after construction); the facade mirrors it so the
        /// selector shows the default instead of "no model".
        model: Option<pi::types::Model>,
        /// The approval mode persisted in the session sidecar (fresh
        /// sessions default to AutoPilot); the facade mirrors it for the
        /// access chip.
        approval_mode: crate::thread::ApprovalMode,
        /// The reasoning effort restored from the session sidecar (fresh
        /// sessions default to High); the facade mirrors it so the model
        /// dropdown shows the restored selection.
        reasoning_effort: crate::language_model::ReasoningEffort,
        /// Plan mode restored from the session sidecar (fresh sessions
        /// default to off); the facade re-syncs rendered instructions.
        plan_mode: bool,
        /// Last plan file recorded in the sidecar, if any.
        plan_file: Option<String>,
        /// A plan review card was pending when the session last settled;
        /// the facade re-emits `PlanReady` so the card re-surfaces.
        plan_review_pending: bool,
        /// Last `UpdatePlan` snapshot persisted in the sidecar; the facade
        /// mirrors it as the rebuild fallback after compaction summarized
        /// the transcript's plan tool calls away.
        plan_snapshot: Option<serde_json::Value>,
        /// Persisted UI annotation cards (sidecar); the facade seeds
        /// `thread.ui_notes` BEFORE emitting `HistoryRestored`, so the
        /// rebuild splices them back at the anchored position
        /// (tool-call-adjacent for approval records).
        ui_notes: Vec<crate::db::UiNoteRecord>,
    },
    /// The turn loop unwound and released the running slot.
    Settled {
        cancelled: bool,
        failed: bool,
        /// Steers confirmed injected during the run.
        steered: Vec<String>,
        /// Steers stranded by an aborted/failed run.
        stranded: Vec<String>,
    },
    /// The display-only preview streamed another batch of the session
    /// transcript into the mirrored history (the authoritative restore is
    /// still running; it replaces the preview at `Ready`). The facade
    /// re-mirrors `history()` and lets the workspace append the newly
    /// available messages.
    ///
    /// parity: the facade re-emits this as `ThreadEvent::HistoryProgress` —
    /// keep the two variants in sync when either side changes.
    HistoryProgress,
    /// The engine's live transcript mirror refreshed mid-run (completed
    /// messages + the streaming partial). The facade re-mirrors its history
    /// silently — no UI event — so a thread switched back to mid-turn
    /// rebuilds from current progress instead of the last settled snapshot.
    LiveHistory,
    /// The session could not be built at all.
    Fatal(anyhow::Error),
    /// A user message landed in the session transcript; the sidebar list
    /// should refresh so the conversation appears at send time, not at
    /// turn end.
    SessionListDirty,
    /// A team tool's round trip: the tool (tokio) sends the op; the facade
    /// (gpui, owns the `Entity<Team>` / member threads) executes it on the
    /// main thread and replies with the model-facing string. Mirrors the
    /// approval-gate round-trip architecture.
    TeamRequest {
        op: TeamOp,
        responder: async_channel::Sender<Result<String, String>>,
    },
    /// A browser tool's host round trip: the tool (tokio) sends the op with
    /// a responder; the facade (gpui, owns the `BrowserHost`) executes it on
    /// the main thread and replies through the channel. Mirrors the
    /// approval-gate round-trip architecture.
    BrowserRequest {
        op: BrowserOp,
        responder: async_channel::Sender<Result<BrowserReply, String>>,
    },
}

/// One team operation a team tool asks the facade to run.
#[derive(Debug, Clone)]
pub enum TeamOp {
    Create {
        name: String,
        members: Vec<MemberSpec>,
    },
    Spawn {
        spec: MemberSpec,
    },
    Send {
        to: String,
        content: String,
    },
    Disband,
    TaskCreate {
        subject: String,
        description: Option<String>,
    },
    TaskList,
    TaskUpdate {
        id: String,
        status: Option<crate::team::TaskStatus>,
        owner: Option<Option<String>>,
        subject: Option<String>,
    },
    TaskGet {
        id: String,
    },
}

/// A worker member to spawn (`TeamCreate` initial roster / `TeamSpawn`).
#[derive(Debug, Clone)]
pub struct MemberSpec {
    pub name: String,
    pub role: String,
    pub prompt: String,
}

/// One browser operation a `web_explore_*` tool asks the host to run.
#[derive(Debug, Clone)]
pub enum BrowserOp {
    Open {
        url: String,
    },
    Navigate {
        id: crate::webview_host::BrowserTabId,
        url: String,
    },
    ReadText {
        id: crate::webview_host::BrowserTabId,
    },
    ReadDom {
        id: crate::webview_host::BrowserTabId,
        selector: Option<String>,
    },
    Click {
        id: crate::webview_host::BrowserTabId,
        selector: String,
    },
    TypeText {
        id: crate::webview_host::BrowserTabId,
        selector: String,
        text: String,
    },
    Scroll {
        id: crate::webview_host::BrowserTabId,
        dx: i32,
        dy: i32,
    },
    Screenshot {
        id: crate::webview_host::BrowserTabId,
    },
    YieldToUser {
        id: crate::webview_host::BrowserTabId,
    },
    Close {
        id: crate::webview_host::BrowserTabId,
    },
}

/// The host's reply payload for a [`BrowserOp`].
#[derive(Debug, Clone)]
pub enum BrowserReply {
    /// A newly opened tab's id.
    TabId(crate::webview_host::BrowserTabId),
    /// Textual read result (page text / DOM / screenshot snapshot).
    Text(String),
    /// Unit success (navigate / click / type / scroll / yield / close).
    Unit,
}
/// An owned engine handle plus its notice channel receiver. Spawning the
/// engine returns both; the facade drains the receiver on the gpui thread.
pub struct SpawnedEngine {
    pub engine: Arc<dyn ThreadEngine>,
    pub events: tokio::sync::mpsc::UnboundedReceiver<BackendNotice>,
}
