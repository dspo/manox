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
use crate::language_model::{AnyLanguageModel, TokenUsage};
use crate::message::Message;

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

    /// The model the backend currently runs, if any.
    fn model(&self) -> Option<AnyLanguageModel>;

    /// Start a turn with the given user text. Events flow back through the
    /// engine's event channel (spawned with the engine); settlement lands in
    /// `history`.
    fn run(&self, prompt: String);

    /// Inject a steer into the running turn. Returns the steer id, which
    /// `cancel_steer` accepts to retract it before the loop drains it.
    fn steer(&self, text: String) -> String;

    /// Retract a queued steer by id. False when it already drained.
    fn cancel_steer(&self, id: &str) -> bool;

    /// Abort the running turn.
    fn abort(&self);

    /// Hot-swap the model for the next provider request.
    fn set_model(&self, model: AnyLanguageModel);

    /// Map the reasoning effort onto the backend's thinking level.
    fn set_thinking_level(&self, level: Option<String>);

    /// Re-point the backend at an existing session file.
    fn open_session(&self, path: PathBuf);

    /// Create a fresh session in the given directory.
    fn new_session(&self, cwd: PathBuf);

    /// The session file the backend currently drives, if any.
    fn active_session_path(&self) -> Option<PathBuf>;

    /// The sessions the backend can list (sidebar source), newest first.
    fn session_list(&self) -> Vec<ThreadSummary>;

    /// Ask the backend to wind down (close its session, stop its runtime).
    /// Default is a no-op for backends that need no explicit shutdown.
    fn shutdown(&self) {}
}

/// Notices the backend sends back to the facade's gpui drainer.
pub enum BackendNotice {
    /// A run event already adapted into a `ThreadEvent`.
    Event(Box<crate::thread::ThreadEvent>),
    /// The session finished building/restoring.
    Ready { restored: bool },
    /// The turn loop unwound and released the running slot.
    Settled {
        cancelled: bool,
        failed: bool,
        /// Steers confirmed injected during the run.
        steered: Vec<String>,
        /// Steers stranded by an aborted/failed run.
        stranded: Vec<String>,
    },
    /// The session could not be built at all.
    Fatal(anyhow::Error),
}

/// An owned engine handle plus its notice channel receiver. Spawning the
/// engine returns both; the facade drains the receiver on the gpui thread.
pub struct SpawnedEngine {
    pub engine: Arc<dyn ThreadEngine>,
    pub events: tokio::sync::mpsc::UnboundedReceiver<BackendNotice>,
}
