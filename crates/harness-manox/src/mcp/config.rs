//! MCP configuration (`mcp.toml` + plugin `.mcp.json` layers).
//!
//! The implementation moved to `agent::mcp::config` (shared with the pi
//! harness); this module keeps the archived import path alive.

pub use agent::mcp::config::*;
