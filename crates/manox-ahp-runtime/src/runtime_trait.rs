//! What the AHP runtime needs from the session runtime.
//!
//! The AHP host's adapter (this crate's [`crate::ahp`]) must drive real
//! sessions: create them, submit turns, steer, settle approvals, read journals,
//! own terminals. Those operations belong to the session runtime — which, while
//! v2 lives, is the gateway in `manox-session-core`.
//!
//! This trait is the seam. The adapter speaks it and nothing else, so:
//!
//! - the session side can be the v2 gateway today and something leaner after v2
//!   is deleted, with no change here; and
//! - the dependency runs one way. The runtime half does not reach into the
//!   gateway's internals (it cannot: it never names its types), and the gateway
//!   implements this interface rather than the reverse.
//!
//! The DTOs below are plain data deliberately. They were the gateway's types
//! (`SessionIntent`, `ForkIntent`) when this code lived beside it; owning them
//! here is what lets the gateway be deleted without touching the adapter.

use manox_ahp::error::HostError;
use manox_journal::ModelRef;
use serde_json::Value;

use crate::error::RuntimeError;

/// How to create one session.
#[derive(Debug, Clone, Default)]
pub struct SessionIntent {
    /// The id the caller chose, when it chose one.
    pub session_id: Option<String>,
    /// Initial working directory.
    pub cwd: Option<String>,
    /// Project binding.
    pub project: Option<String>,
    /// Initial model, as the canonical `provider/model` reference (L8).
    pub initial_model: Option<ModelRef>,
    /// Approval mode for the new session.
    pub approval_mode: Option<String>,
    /// Reasoning effort for the new session.
    pub reasoning_effort: Option<String>,
    /// Hidden context blocks written as non-displaying messages before the
    /// first turn.
    pub seed: Option<Vec<Value>>,
    /// Ordered extra working directories granted to the session (multi-root);
    /// each joins the granted-root set before the engine materializes. Empty
    /// keeps single-cwd behaviour.
    pub working_directories: Vec<String>,
}

/// How to fork one session.
#[derive(Debug, Clone)]
pub struct ForkIntent {
    /// The session to copy from.
    pub source_session_id: String,
    /// Copy the source's active chain through this journal entry.
    pub through_entry_id: String,
    /// The id the fork must land on, when the caller chose one (AHP's
    /// `createChat` names the chat URI up front). `None` mints one.
    pub target_session_id: Option<String>,
    /// Working directory for the fork; the source's when absent.
    pub cwd: Option<String>,
    /// Project binding for the fork.
    pub project: Option<String>,
    /// Initial model for the fork.
    pub initial_model: Option<ModelRef>,
    /// Approval mode for the fork.
    pub approval_mode: Option<String>,
    /// Reasoning effort for the fork.
    pub reasoning_effort: Option<String>,
}

/// One live terminal's snapshot, in the shapes the AHP terminal channel serves.
#[derive(Debug, Clone)]
pub struct TerminalSnapshot {
    /// Terminal title (may be empty).
    pub title: String,
    /// Working directory the terminal was spawned in.
    pub cwd: Option<String>,
    /// Grid width in columns.
    pub cols: i64,
    /// Grid height in rows.
    pub rows: i64,
    /// The visible grid, one string per row.
    pub lines: Vec<String>,
    /// The process exit code, when it has exited.
    pub exit_code: Option<i64>,
    /// The session this terminal belongs to.
    pub session_id: String,
}

/// The session runtime behind the AHP adapter.
///
/// Every method is fallible with [`RuntimeError`]: a capability the runtime
/// cannot honour is a refusal the caller reports, never a silent no-op.
pub trait SessionRuntime: Send + Sync + 'static {
    /// The live terminal's snapshot, when this runtime owns that terminal.
    fn terminal_state(&self, terminal_id: &str) -> Option<TerminalSnapshot>;

    /// The raw PTY byte stream of a terminal, for the `terminal/data` pump.
    fn terminal_raw_tap(
        &self,
        terminal_id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<std::sync::Arc<Vec<u8>>>>;

    /// Spawn (or re-attach) a terminal under a caller-chosen id.
    fn create_terminal(
        &self,
        session_id: &str,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), RuntimeError>;

    /// Release a terminal and its PTY.
    fn dispose_terminal(&self, terminal_id: &str) -> Result<(), RuntimeError>;

    /// Forward keystrokes to a terminal's PTY.
    fn terminal_input(&self, terminal_id: &str, data: &str) -> Result<(), RuntimeError>;

    /// Resize a terminal's PTY grid.
    fn terminal_resize(&self, terminal_id: &str, cols: u16, rows: u16) -> Result<(), RuntimeError>;

    /// Create (or adopt) a session.
    fn create_session(&self, owner: &str, intent: SessionIntent) -> Result<(), RuntimeError>;

    /// Fork a session's journal through an entry.
    fn fork_session(&self, owner: &str, intent: ForkIntent) -> Result<(), RuntimeError>;

    /// Dispose a session and release its live resources.
    fn dispose_session(&self, owner: &str, session_id: &str) -> Result<(), RuntimeError>;

    /// Whether the runtime currently drives this session.
    fn has_session(&self, session_id: &str) -> bool;

    /// Submit a user turn.
    fn submit(&self, owner: &str, session_id: &str, text: String) -> Result<Value, RuntimeError>;

    /// Inject a steer into the running turn (or park it when idle).
    fn steer(
        &self,
        session_id: &str,
        message_id: &str,
        text: String,
    ) -> Result<Value, RuntimeError>;

    /// Withdraw a parked follow-up.
    fn drop_queued(&self, session_id: &str, message_id: &str);

    /// Cancel the session's running turn.
    fn cancel_turn(&self, session_id: &str) -> Result<(), RuntimeError>;

    /// Select the session's model.
    fn set_model(&self, session_id: &str, model: &str);

    /// Select the session's reasoning effort.
    fn set_reasoning_effort(&self, session_id: &str, effort: &str);

    /// Select the session's approval mode.
    fn set_approval_mode(&self, session_id: &str, mode: &str);

    /// Move the session's effective working directory.
    fn set_cwd(&self, session_id: &str, cwd: &str) -> Result<(), RuntimeError>;

    /// Archive or unarchive the session.
    fn archive_session(&self, owner: &str, session_id: &str, archived: bool);

    /// Rename the session to a user title.
    fn rename_session(&self, session_id: &str, title: &str) -> RenameOutcome;

    /// Pin or unpin the session.
    fn pin_session(&self, session_id: &str, pinned: bool) -> bool;

    /// Move the session in the sidebar order.
    fn order_session(&self, session_id: &str, before: Option<&str>) -> bool;

    /// Compact the session's history.
    fn compact(&self, session_id: &str, instructions: Option<String>);

    /// Seed plan execution after a verdict.
    fn plan_seed(&self, session_id: &str, plan_file: &str);

    /// Settle a tool-call confirmation.
    fn confirm_tool_call(
        &self,
        session_id: &str,
        auth_id: &str,
        approved: bool,
    ) -> Result<(), RuntimeError>;

    /// Settle a question card.
    fn answer_question(&self, session_id: &str, request_id: &str) -> Result<(), RuntimeError>;

    /// The AHP-facing failure for a runtime failure.
    fn host_error(error: RuntimeError) -> HostError
    where
        Self: Sized,
    {
        HostError::Backend(error.message)
    }
}

/// What a rename did.
///
/// Three outcomes rather than a bool, because a caller has to tell "the title
/// was rejected" from "the title could not be made durable" — only the first is
/// the client's fault, and only the second is worth retrying. Folding them made
/// a refusal report the blank-title message for a perfectly valid title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameOutcome {
    /// Stored, and both durable legs accepted it.
    Renamed,
    /// Nothing to rename: the title was empty or whitespace-only.
    Blank,
    /// No such session.
    UnknownSession,
    /// The in-memory row changed, but the journal row had nowhere to land, so
    /// the rename is not durable and must not be reported as success.
    NotPersisted,
}
