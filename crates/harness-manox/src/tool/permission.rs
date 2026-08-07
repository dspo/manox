//! Tool-call permissions (re-exported from the agent crate).
//!
//! The canonical definitions moved to `agent::permission` so the live pi
//! harness approval gate and the retired manox harness share one currency.

pub use agent::permission::{
    PendingAuthMeta, PermissionCache, PermissionDecision, ToolAuthorizationResponse,
};
