//! The [`SubagentProvider`] seam — one registered transport for running
//! child agents (the dsh interface of the same name). Providers are trusted
//! same-process implementations; the runtime treats descriptors and
//! returned values as borrowed immutable data. The runtime may call one
//! provider concurrently for distinct children; providers isolate
//! operation-local mutable state.
//!
//! [`RunObserver`] is the host bridge: the run loop lives with the provider
//! (dsh parity — a provider owns its children's turns), while everything
//! that touches host-owned surfaces (transcript events, health surfaces,
//! background cards) flows through the observer the host injected at
//! assembly.

use async_trait::async_trait;
use futures::FutureExt;
use tokio_util::sync::CancellationToken;

use crate::ext::subagent::types::ResolvedStartRequest;
use crate::ext::subagent::types::{Capabilities, RunEndInfo, RunInfo, RunResult, SubagentError};

/// One one-shot child handle returned after publication. Prompt submission,
/// turn work, and infrastructure faults after that boundary belong to
/// [`SubagentRun::result`]. Consumers await that result; background
/// consumers must also honor [`SubagentRun::dispose`] to cancel remaining
/// work. A run is one delegation with one result.
#[derive(Debug, Clone)]
pub struct SubagentRun {
    /// Parent-scoped run id (minted by the runtime).
    pub id: String,
    dispose: CancellationToken,
    result: futures::future::Shared<futures::future::BoxFuture<'static, RunResult>>,
}

impl SubagentRun {
    pub fn new(
        id: String,
        dispose: CancellationToken,
        result: futures::future::BoxFuture<'static, RunResult>,
    ) -> Self {
        SubagentRun {
            id,
            dispose,
            result: result.shared(),
        }
    }

    /// Resolves with the child's terminal [`RunResult`] when the run
    /// settles. Does not reject on a child-level failure — a model or
    /// transport failure resolves with [`StopReason::Error`](crate::ext::subagent::types::StopReason).
    pub async fn result(&self) -> RunResult {
        self.result.clone().await
    }

    /// The shared settled-result future, for observers that watch the run
    /// without consuming it.
    pub fn clone_result_future(
        &self,
    ) -> futures::future::Shared<futures::future::BoxFuture<'static, RunResult>> {
        self.result.clone()
    }

    /// The cancellation token that terminates the run's remaining work.
    pub fn dispose_token(&self) -> &CancellationToken {
        &self.dispose
    }

    /// Cancel remaining work and reach child quiescence. Idempotent.
    pub fn dispose(&self) {
        self.dispose.cancel();
    }
}

/// What the continuation manager (future) would ask a provider for while
/// materializing one continuable child's first activation. Reserved seam —
/// no in-process backend implements it yet.
pub struct ContinuableCreateRequest {
    /// The reserved durable child session id, for provider diagnostics.
    pub session_id: String,
    /// Caller cancellation, owning preparation only until the initial
    /// prompt is accepted into the child's inbox.
    pub cancel: CancellationToken,
}

/// A provider's detached contribution to one continuable child's creation:
/// only whether the child session is seeded with parent history. DATA, never
/// a capability — the manager owns every later operation.
pub struct ContinuableCreateSpec {
    /// Completed-turn prefix of the parent's history to seed the child with,
    /// or `None` for a fresh child.
    pub seed: Option<Vec<crate::core::session::SessionTreeEntry>>,
}

/// The host-side observer bridge. The ext run loop calls these at the same
/// lifecycle points the retired host-side run task drove its `BackendNotice`
/// sends from; a host implementation folds child events into its transcript
/// surfaces, health watchdog, and background cards.
pub trait RunObserver: Send + Sync {
    /// The run was published and its session built; surfaces may open a row.
    fn on_start(&self, info: &RunInfo) {
        let _ = info;
    }

    /// One child session event (streamed deltas, tool calls, turn ends).
    fn on_child_event(&self, run_id: &str, event: &crate::core::types::AgentEvent) {
        let _ = (run_id, event);
    }

    /// Fixed-cadence health tick. Returns `true` to enforce a stall — the
    /// run loop terminates the child and settles it as an error with the
    /// salvaged partial output. Reporting (state flips, health lines) is the
    /// implementation's job; the loop only asks "kill?".
    fn on_tick(&self, run_id: &str) -> bool {
        let _ = run_id;
        false
    }

    /// The run settled; surfaces may close the row with the terminal state.
    fn on_settled(&self, end: &RunEndInfo) {
        let _ = end;
    }
}

/// One registered transport for running child agents.
#[async_trait]
pub trait SubagentProvider: Send + Sync {
    /// Unique registry name (e.g. `spawn`).
    fn name(&self) -> &str;

    /// The start-time features this provider supports.
    fn capabilities(&self) -> Capabilities;

    /// Whether the child sees the parent's completed-turn prefix.
    /// Descriptive only — the model-facing tool derives truthful wording
    /// from it; it says nothing about tool registration or authority.
    fn inherits_parent_context(&self) -> bool {
        false
    }

    /// Establish a ONE-SHOT child and return its handle after publication.
    /// The runtime has already validated every requested capability and
    /// resolved `request.descriptor`, so a session-backed implementation
    /// persists that descriptor inside the child's header. Before
    /// fulfillment, the provider owns setup and cleans any unpublished
    /// partial resources; ownership transfers on fulfillment.
    async fn start(&self, request: ResolvedStartRequest) -> Result<SubagentRun, SubagentError>;

    /// Whether this provider can create continuable children (dsh: method
    /// presence IS the capability; in Rust the predicate stands in for it).
    fn supports_continuable(&self) -> bool {
        false
    }

    /// Contribute the detached creation inputs for one continuable child.
    /// The default implementation rejects — only providers that return
    /// `true` from [`SubagentProvider::supports_continuable`] may be asked.
    async fn prepare_continuable(
        &self,
        _request: ContinuableCreateRequest,
    ) -> Result<ContinuableCreateSpec, SubagentError> {
        Err(SubagentError::UnsupportedCapability {
            provider: self.name().to_string(),
            capability: "continuable",
        })
    }
}
