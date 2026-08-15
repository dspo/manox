//! Transport-agnostic agent actor behind the VS Code napi binding.
//!
//! Hosts the gpui `HeadlessAppContext` and one `Thread` per session, and
//! speaks a JSON command/event protocol. The napi crate wraps this in a
//! Node-facing surface; any other transport (stdio, websocket) could wrap it
//! the same way.

pub mod actor;
pub mod events;
pub mod model_chat;
