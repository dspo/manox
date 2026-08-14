//! napi-rs bindings exposing the manox agent core to a TypeScript host.
//!
//! P0 spike surface: start the agent actor thread (gpui `HeadlessAppContext`
//! on a dedicated thread) and stream `ThreadEvent`s back to Node.

#[macro_use]
extern crate napi_derive;

mod actor;
mod events;

use std::sync::OnceLock;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunction;

static ACTOR: OnceLock<actor::ActorHandle> = OnceLock::new();

/// Smoke-test export: verifies the native module loads and links the agent
/// dependency graph.
#[napi]
pub fn ping() -> String {
    "pong".to_string()
}

/// Start the agent actor thread. `event_cb` receives serialized event JSON
/// strings; it is invoked from the actor thread, so it must not block.
#[napi]
pub fn start(event_cb: JsFunction) -> Result<()> {
    let tsfn: ThreadsafeFunction<Vec<String>> =
        event_cb.create_threadsafe_function(0, |ctx| Ok(vec![ctx.value]))?;
    let handle = actor::start(tsfn)?;
    ACTOR
        .set(handle)
        .map_err(|_| napi::Error::from_reason("actor already started"))?;
    Ok(())
}

/// Deliver a command (`{"cmd": "...", ...}`) to the agent actor thread.
#[napi]
pub fn send_command(command: String) -> Result<()> {
    match ACTOR.get() {
        Some(handle) => handle.send(command),
        None => Err(napi::Error::from_reason(
            "actor not started; call start(callback) first",
        )),
    }
}
