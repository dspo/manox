//! napi-rs bindings exposing the manox agent core to a TypeScript host.
//!
//! Thin glue over `manox-actor`: the host starts one agent actor thread and
//! streams session-tagged events back to Node. `shutdown()` tears the actor
//! down so window reloads and `deactivate` re-initialize cleanly.

#[macro_use]
extern crate napi_derive;

use std::sync::Mutex;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

static ACTOR: Mutex<Option<manox_actor::actor::ActorHandle>> = Mutex::new(None);

fn actor_slot() -> std::sync::MutexGuard<'static, Option<manox_actor::actor::ActorHandle>> {
    ACTOR.lock().unwrap_or_else(|e| e.into_inner())
}

/// Smoke-test export: verifies the native module loads and links the agent
/// dependency graph.
#[napi]
pub fn ping() -> String {
    "pong".to_string()
}

/// Start the agent actor thread. `event_cb` receives one serialized event
/// JSON string per call; it runs on the Node main thread, scheduled by the
/// actor through a threadsafe function.
#[napi]
pub fn start(event_cb: JsFunction) -> Result<()> {
    // The built-in Chrome engine (rustwright-core, via ChromeUse) is NOT
    // linked into the VS Code host: the agent is built without the
    // `chrome-use` feature on the manox-napi edge, so nothing here can ever
    // launch it. DISABLE_TELEMETRY is still set as defense in depth in case
    // the engine is ever enabled on this host.
    unsafe { std::env::set_var("DISABLE_TELEMETRY", "1") };
    let mut slot = actor_slot();
    if slot.is_some() {
        return Err(napi::Error::from_reason("actor already started"));
    }
    let tsfn: ThreadsafeFunction<String> =
        event_cb.create_threadsafe_function(0, |ctx| Ok(vec![ctx.value]))?;
    let sink = manox_actor::actor::EventSink::new(move |json| {
        let _ = tsfn.call(Ok(json), ThreadsafeFunctionCallMode::NonBlocking);
    });
    let handle = manox_actor::actor::start(sink)
        .map_err(|e| napi::Error::from_reason(format!("failed to start actor: {e}")))?;
    *slot = Some(handle);
    Ok(())
}

/// Deliver a command (`{"cmd": "...", ...}`) to the agent actor thread.
#[napi]
pub fn send_command(command: String) -> Result<()> {
    match actor_slot().as_ref() {
        Some(handle) => handle.send(command).map_err(napi::Error::from_reason),
        None => Err(napi::Error::from_reason(
            "actor not started; call start(callback) first",
        )),
    }
}

/// Tear down the agent actor thread and release the event callback. Safe to
/// call when the actor is not started; intended for host `deactivate`.
#[napi]
pub fn shutdown() {
    if let Some(handle) = actor_slot().take() {
        handle.shutdown();
    }
}
