//! Context-free session orchestration core.
//!
//! Drives gpui-free `ThreadHandle`s through `session::handle_command` for
//! any host — the napi/vscode actor shell or the WebUI bridge. The core
//! owns no global state beyond the shared `agent` handles, so one
//! `ActorState` runs against either host without feature unification
//! pulling the agent `test-support` feature into a release build.
//!
//! The `session` module is the command/event engine (thread handles, event
//! pumps, store bookkeeping); `events` projects `ThreadEvent`s onto the
//! wire JSON; `model_chat` is the stateless bare-model completion channel
//! shared with the VS Code language-model provider.

pub mod events;
pub mod model_chat;
pub mod session;
