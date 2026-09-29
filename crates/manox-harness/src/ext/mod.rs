// In-process extensions for the pi harness core.
//
// The core `crates/pi` defines the seams (`BashOperations`,
// `BackgroundTaskRegistry`); this crate implements them stack-internally —
// no dynamic loading, no out-of-process runtime — and assembles the
// product-level bash tool on top.

pub mod bash;
pub mod model_ref;
pub mod monitor;
pub mod path_selector;
pub mod prompt;
pub mod provider;
pub mod read;
pub mod sandbox;
pub mod session_meta;
pub mod session_stream;
pub mod steer_bus;
pub mod subagent;

pub use bash::background::{BackgroundRegistry, BashOutputTool, TaskStopTool};
pub use bash::persistent::PersistentShellOperations;
pub use monitor::MonitorTool;
pub use subagent::SubagentRuntime;

/// Process-global ordinal for background task ids. Every registry
/// (`BackgroundRegistry`, `WsMonitorRegistry`) draws from this one counter so
/// ids stay unique across concurrently-live sessions in one process; the
/// host's task registry is process-global and keys on these ids.
pub(crate) fn next_task_ordinal() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
