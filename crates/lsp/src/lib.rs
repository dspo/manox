//! Lightweight LSP client for manox.
//!
//! Lazily spawns an already-installed language server (rust-analyzer / gopls /
//! pyright / typescript-language-server) as a child process via the
//! `supervisor` process bus, speaks JSON-RPC over stdio, and exposes
//! code-intel requests. The wire framer is hand-rolled (`proto.rs`); `lsp-types`
//! supplies typed params/results only.
//!
//! This crate is pure tokio — no `agent`/`gpui` dependency — so the JSON-RPC
//! framer and client stay unit-testable without the GPUI runtime. The
//! `AgentTool` adapters that wrap these clients live in the `agent` crate
//! (`agent::lsp`), avoiding a dependency cycle.

pub mod client;
pub mod proto;
pub mod registry;
pub mod spec;

pub use client::{LspClient, ServerStatus};
pub use registry::{LspRegistry, global, init, try_global};
pub use spec::{LspServerSpec, SPECS, spec_for_extension, spec_for_id};
