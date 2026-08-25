//! Transport-agnostic agent actor behind the VS Code napi binding.
//!
//! Hosts the gpui `HeadlessAppContext` and the shutdown sentinel, and
//! delegates session orchestration to `manox-session-core`. The napi crate
//! wraps this in a Node-facing surface; any other transport (stdio,
//! websocket) could wrap it the same way.

pub mod actor;
