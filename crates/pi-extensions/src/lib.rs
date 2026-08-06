// In-process extensions for the pi harness core.
//
// The core `crates/pi` defines the seams (`BashOperations`,
// `BackgroundTaskRegistry`); this crate implements them stack-internally —
// no dynamic loading, no out-of-process runtime — and assembles the
// product-level bash tool on top.

pub mod agents;
pub mod bash;
pub mod provider;
pub mod session_meta;

pub use agents::SubagentTool;
pub use bash::background::{BackgroundRegistry, BashOutputTool, TaskStopTool};
pub use bash::persistent::PersistentShellOperations;
