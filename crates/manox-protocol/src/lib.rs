//! manox-protocol — transport-agnostic, bidirectional RPC between the manox
//! agent backend and its frontends.
//!
//! The same wire vocabulary serves every transport: an in-process channel
//! (the gpui desktop app), a WebSocket (the browser host), napi (VS Code), and
//! a future tauri command bridge. Two primitives, each usable in both
//! directions, expressed as serde-tagged enums (JSON-RPC 2.0 semantics without
//! string method names, keeping full type safety):
//!
//! - **Request/Response** — carries an [`MsgId`] and expects a reply.
//!   Client→server: queries ([`ClientCall`]). Server→client: adjudication and
//!   capability calls ([`ServerCall`]).
//! - **Notification** — no id, no reply. Client→server: fire-and-forget
//!   commands ([`ClientNote`]). Server→client: streaming updates
//!   ([`ServerNote`]).
//!
//! A client answers a [`ServerCall`] with [`FromClient::Reply`]; a server
//! answers a [`ClientCall`] with [`FromServer::Response`]. Id correlation,
//! timeouts and cancellation are handled by [`RpcPeer`] over any
//! [`RpcConnection`].

pub mod base64_bytes;
pub mod client;
pub mod handshake;
pub mod msg;
pub mod server;
pub mod transport;
pub mod wire;

pub use client::{ClientCall, ClientNote, ImageAttachment};
pub use handshake::{ClientHello, HookKind, Initialize};
pub use msg::{FromClient, FromServer, MsgId, RpcError};
pub use server::{ServerCall, ServerNote};
pub use transport::{InProcessConnection, RpcConnection, RpcPeer, in_process_pair};
pub use wire::{
    ModelInfo, ThreadListItem, WireContentBlock, WireMessage, WireMessageAuthor,
    WireMessageProvenance, WireMessageUi, WireRole, WireToolResult, WireToolUse,
};
