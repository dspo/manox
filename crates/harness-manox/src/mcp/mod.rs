//! MCP (Model Context Protocol) client integration — retired manox layer.
//!
//! The connection core and config moved to `agent::mcp` (shared with the pi
//! harness, initialized by `agent::init`). This module keeps the manox
//! `AgentTool` bridge ([`tool::McpTool`]) and the manox-shaped registry that
//! `tools::main_registry` consumes.

pub mod config;
pub mod registry;
pub mod tool;

pub use registry::{McpRegistry, global as registry_global, init as registry_init};
