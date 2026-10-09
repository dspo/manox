// Coding-agent non-UI facade: a minimal vertical path from a working
// directory to a live AgentSession — resources, model/credential resolution,
// the session repository, and the harness — without duplicating loop /
// compaction / session implementations. The facade reads no configuration
// from disk; consumers inject everything through the builder.

pub mod agent_session;
pub mod model_runtime;
pub mod resource_loader;
pub mod usage;

pub use agent_session::{AgentSession, AgentSessionBuilder};
pub use model_runtime::ModelRuntime;
pub use resource_loader::ResourceLoader;
pub use usage::{ModelUsageBreakdown, SessionStats, UsageTotals};

/// Convenience factory: a builder wired to the current directory with the
/// default tool set and a model runtime that reads credentials from the
/// environment.
pub fn create_agent_session() -> AgentSessionBuilder {
    AgentSessionBuilder::default()
}
