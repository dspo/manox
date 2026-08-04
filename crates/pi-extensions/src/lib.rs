// In-process extensions for the pi harness core.
//
// The core `crates/pi` defines the seams (`BashOperations`,
// `BackgroundTaskRegistry`); this crate implements them stack-internally —
// no dynamic loading, no out-of-process runtime — and assembles the
// product-level bash tool on top.

pub mod bash;

pub use bash::background::{BackgroundRegistry, BashOutputTool, TaskStopTool};
pub use bash::persistent::PersistentShellOperations;
