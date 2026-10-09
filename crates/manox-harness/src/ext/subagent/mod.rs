//! Subagent dispatch — the runtime seam that turns a delegation request
//! into a running child agent:
//!
//! - [`SubagentRuntime`] is the provider registry + capability-checked
//!   one-shot start + live-run table + the `start`/`end` observation feed.
//! - [`SubagentProvider`] is one registered transport;
//!   [`spawn::SpawnProvider`] is the in-process fresh-session backend.
//! - [`descriptor::Descriptor`] is the versioned, model-invisible child
//!   identity record persisted in the child session header.
//! - [`provider::RunObserver`] is the host bridge for transcript/health
//!   surfaces.
//!
//! Continuable children (durable multi-turn sessions with an inbox) are
//! reserved on the seam — the trait predicates and the runtime's
//! `start_continuable` exist and reject — so the future manager lands
//! without reshaping this surface.

pub mod descriptor;
pub mod provider;
pub mod runtime;
pub mod spawn;
pub mod types;

pub use descriptor::Descriptor;
pub use provider::{
    ContinuableCreateRequest, ContinuableCreateSpec, RunObserver, SubagentProvider, SubagentRun,
};
pub use runtime::{ProviderGuard, SubagentRuntime};
pub use spawn::{
    SpawnProvider, Worktree, auto_deny_gated, explore_agent_def, extract_final_text,
    register_defaults, sailor_agent_def,
};
pub use types::{
    AgentOptions, Budgets, Capabilities, DelegationToolConfig, Event, HUMAN_INTERACTION_TOOLS,
    MIN_BUDGET_MS, RunEndInfo, RunInfo, RunResult, RunSnapshot, StartRequest, StopReason,
    SubagentError, ToolFilter, test_request,
};
