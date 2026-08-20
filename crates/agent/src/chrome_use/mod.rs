//! ChromeUse — agent tool set that drives a real Chrome/Chromium through the
//! in-process rustwright CDP engine (the workspace `rustwright-core`
//! dependency; no driver subprocess, so the single-binary deliverable holds).
//!
//! Interaction model: accessibility-style snapshots with element refs
//! (`e1`, `e2`, …). Tools act through the refs issued by the tab's latest
//! snapshot; every write action replies with a fresh snapshot so the loop
//! continues without an extra round trip. Refs are never reused across
//! snapshots.
//!
//! The runtime is a process-wide singleton — one Chrome session shared across
//! threads. Reads (`Snapshot` / `WaitFor` / `Screenshot`) are approval-free
//! and read-only; writes ride the owning thread's approval mode like `Bash`
//! / `Write`. Chrome's network egress bypasses the bash sandbox proxy.

mod bridge;
mod runtime;
mod snapshot;
mod tools;

pub use runtime::{ChromeTabId, shutdown};
pub use tools::{
    ChromeUseClickTool, ChromeUseCloseTool, ChromeUseEvaluateTool, ChromeUseNavigateTool,
    ChromeUseOpenTool, ChromeUsePressKeyTool, ChromeUseScreenshotTool, ChromeUseScrollTool,
    ChromeUseSelectOptionTool, ChromeUseSnapshotTool, ChromeUseTabsTool, ChromeUseTypeTool,
    ChromeUseWaitForTool,
};
