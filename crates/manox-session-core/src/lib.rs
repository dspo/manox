//! Context-free session orchestration core.
//!
//! Drives `Entity<Thread>`s through `session::handle_command` on whatever
//! `App` the host provides — a `HeadlessAppContext` closure in `manox-actor`
//! (napi/vscode) or the app main thread in the WebUI bridge. The core owns no
//! global state, so one `ActorState` runs against either host without feature
//! unification pulling the gpui/agent `test-support` features into a release
//! build.
//!
//! The `session` module is the command/event engine (thread entities,
//! subscriptions, store bookkeeping); `events` projects `ThreadEvent`s onto
//! the wire JSON; `model_chat` is the stateless bare-model completion channel
//! shared with the VS Code language-model provider.

pub mod events;
pub mod model_chat;
pub mod session;
