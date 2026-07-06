//! MCP (Model Context Protocol) client integration.
//!
//! Reads `~/.config/cx/manox/mcp.toml`, connects each configured server (stdio
//! or streamable HTTP) via the `rmcp` SDK, lists its tools, and exposes them
//! as `AgentTool`s that route `tools/call` back through the rmcp client. No UI
//! — configuration is file-only.

pub mod config;
pub mod registry;
pub mod tool;

pub use registry::{McpRegistry, global as registry_global, init as registry_init};
