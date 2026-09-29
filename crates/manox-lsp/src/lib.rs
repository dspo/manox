//! Lightweight LSP client library for host apps.
//!
//! Lazily spawns an already-installed language server (rust-analyzer / gopls /
//! pyright / typescript-language-server) as a child process via the
//! `supervisor` process bus, speaks JSON-RPC over stdio, and exposes
//! code-intel requests. The wire framer is hand-rolled (`proto.rs`); `lsp-types`
//! supplies typed params/results only.
//!
//! This crate is a standalone building block: pure tokio — no `agent`/`gpui`
//! dependency — so the JSON-RPC framer and client stay unit-testable without
//! the GPUI runtime. Nothing inside the manox workspace integrates it; the
//! host app decides whether and how to surface LSP capability — by wrapping
//! these clients in its own agent tools, or (over AHP) contributing them via
//! `session/activeClientSet` (which lands in the runtime's
//! `register_client_tools`) or, in-process, via
//! `manox_agent::embedder_tools::set_provider`.

pub mod client;
pub mod proto;
pub mod registry;
pub mod spec;

pub use client::{DiagnosticsReport, LspClient, ServerStatus};
pub use registry::{LspRegistry, global, init, try_global};
pub use spec::{LspServerSpec, SPECS, spec_for_extension, spec_for_id};
