//! Process bus: owns the lifecycle of every third-party process manox spawns
//! (LSP servers, MCP stdio servers, …).
//!
//! Each child is launched in its own process group (`Command::process_group(0)`)
//! so a single `kill(-pgid, sig)` reaps the whole tree — the child plus any
//! grandchildren it forks (e.g. a shell that spawns more). A per-child graceful
//! hook runs before the signal fallback so clients like LSP can send a proper
//! `shutdown`/`exit` first.
//!
//! There is no OS-level "register this PID as my child after the fact" API on
//! Unix — parentage is fixed at `fork` time, and manox is the spawner, so it is
//! already the parent. Process-group `kill` is the portable Unix reaping tool;
//! Windows Job Objects are the analog (not built here, manox is darwin-only —
//! see issue #128).
//!
//! Global access mirrors manox's other registries: `global()` lazily inits a
//! process-wide `ProcessBus`. `shutdown_all()` is manox's exit hook.

mod bus;
mod proc;

pub use bus::{Condition, ProcessBus, SpawnedProcess};
pub use proc::{ManagedProcess, ProcessKind};

use std::sync::OnceLock;

static BUS: OnceLock<ProcessBus> = OnceLock::new();

/// The process-wide bus. Lazily initialized on first use — no `init` needed.
pub fn global() -> &'static ProcessBus {
    BUS.get_or_init(ProcessBus::new)
}
