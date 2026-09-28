//! The `Backend` seam: everything the AHP host needs from the runtime.
//!
//! The host owns protocol mechanics only. Sessions, journals, turns, approvals,
//! terminals and the `resource*` file plane belong to the runtime, which
//! implements this trait (`manox-session-core`). Two rules keep the seam thin
//! and the layering honest:
//!
//! - **Seeding is a fold.** [`Backend::session_state`] / [`Backend::chat_state`]
//!   answer from the durable journal; the host caches the result and then
//!   advances it with the action envelopes it publishes. Host state and client
//!   state therefore reduce the same stream (the convergence gate in `tests/`).
//! - **Side effects follow acceptance.** [`Backend::dispatch`] runs only after
//!   the acceptance table and the reducer accepted an action, so a refused write
//!   never starts runtime work.

use ahp_types::actions::{ActionOrigin, StateAction};
use ahp_types::commands::{CreateChatParams, CreateSessionParams};
use ahp_types::state::{ChatState, RootState, SessionState, SessionSummary, TerminalState, Turn};
use serde_json::Value;

use crate::error::HostError;
use crate::resource::ResourcePlane;

/// What the runtime did with an accepted action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// The runtime took the action; the envelope goes out.
    Accepted,
    /// Accepted by the reducer but nothing to do (idempotent replay).
    Ignored,
    /// The runtime cannot honour it; the host echoes a rejection instead.
    Rejected(String),
}

/// The runtime behind the host.
pub trait Backend: Send + Sync + 'static {
    /// Root channel state: agent + model catalogue, terminal catalogue, config.
    fn root_state(&self) -> RootState;

    /// Session summaries in display order (the host pages them for
    /// `listSessions`).
    fn list_sessions(&self) -> Vec<SessionSummary>;

    /// One session's summary, for `root/sessionAdded` / `…SummaryChanged`.
    fn session_summary(&self, session_id: &str) -> Option<SessionSummary>;

    /// Full session state, folded from the durable journal.
    fn session_state(&self, session_id: &str) -> Option<SessionState>;

    /// Full chat state, folded from the durable journal.
    fn chat_state(&self, chat_id: &str) -> Option<ChatState>;

    /// Which session owns `chat_id` (the host needs it to seed a chat's parent
    /// catalog before it can serve either channel).
    fn session_state_for_chat(&self, chat_id: &str) -> Option<String>;

    /// Terminal state, when the runtime owns the terminal.
    fn terminal_state(&self, terminal_id: &str) -> Option<TerminalState>;

    /// Spawn (or re-attach) a terminal for `session_id` under `terminal_id`.
    ///
    /// The id is the client's, from the channel URI, so a client that names a
    /// terminal can find it again by that name.
    fn create_terminal(
        &self,
        _session_id: &str,
        _terminal_id: &str,
        _cols: u16,
        _rows: u16,
    ) -> Result<(), HostError> {
        Err(HostError::Unimplemented("createTerminal".to_string()))
    }

    /// Dispose a terminal and release its PTY.
    fn dispose_terminal(&self, _terminal_id: &str) -> Result<(), HostError> {
        Err(HostError::Unimplemented("disposeTerminal".to_string()))
    }

    /// Create (or adopt) a session for a client-chosen URI.
    fn create_session(
        &self,
        session_id: &str,
        params: &CreateSessionParams,
    ) -> Result<(), HostError>;

    /// Dispose a session and its journals' live resources.
    fn dispose_session(&self, session_id: &str) -> Result<(), HostError>;

    /// Create a chat (journal) inside a session, optionally forking.
    fn create_chat(
        &self,
        session_id: &str,
        chat_id: &str,
        params: &CreateChatParams,
    ) -> Result<(), HostError>;

    /// Dispose a chat.
    fn dispose_chat(&self, chat_id: &str) -> Result<(), HostError>;

    /// Page older turns into the chat's state (`fetchTurns`).
    fn fetch_turns(
        &self,
        chat_id: &str,
        cursor: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Vec<Turn>, HostError>;

    /// `fetchTurns` with the cursor of the page after this one.
    ///
    /// Snapshots are tail-cut ([`crate::channels::chat::tail_view`]), so paging
    /// is what makes the cut lossless; a backend that cannot page more says so
    /// by answering `None`, which is also this default.
    fn fetch_turns_page(
        &self,
        chat_id: &str,
        cursor: Option<&str>,
        limit: Option<i64>,
    ) -> Result<(Vec<Turn>, Option<String>), HostError> {
        Ok((self.fetch_turns(chat_id, cursor, limit)?, None))
    }

    /// Side effects of an accepted, client-dispatched action.
    fn dispatch(
        &self,
        channel: &str,
        action: &StateAction,
        origin: &ActionOrigin,
    ) -> DispatchOutcome;

    /// The `resource*` plane, when the backend serves one.
    fn resources(&self) -> Option<&dyn ResourcePlane> {
        None
    }

    /// An `x-manox/*` command. The default refuses: the declaration surface
    /// only advertises what the build actually serves.
    fn extension(&self, method: &str, _params: &Value) -> Result<Value, HostError> {
        Err(HostError::Unimplemented(method.to_string()))
    }

    /// Baselines for the stateless extension channels (`x-manox-plan:/…`).
    /// `None` means the runtime has nothing to say for that channel yet.
    fn extension_baseline(&self, _channel: &str) -> Option<(String, Value)> {
        None
    }
}
