//! The rquickjs Runtime encapsulation: one execution = one fresh `std::thread`
//! with one fresh Runtime.
//!
//! The thread owns the Runtime exclusively (rquickjs is `!Send` without the
//! `parallel` feature, and the host must never run the VM on its own thread —
//! a pure-compute script would stall the host's event loop). The thread body
//! creates the Runtime, installs the guardrails, wires the bridge natives into
//! the prelude, and drives the VM:
//!
//! - drain the promise job queue, checking the interrupt flag between jobs;
//! - when the queue is empty and the script has not finished, fail immediately
//!   if no tool call is pending (hang detection — with no timers or IO in the
//!   VM, nothing could ever resume it) and otherwise block on the result
//!   channel with `recv_timeout`, stepping the interrupt flag;
//! - results are settled into the pending promises out of order; the call id
//!   pairs them back to the right continuation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use rquickjs::context::EvalOptions;
use rquickjs::{Coerced, Context, Function, Object, Persistent, Value};

use crate::bridge::{DoneMessage, HostToVm, VmToHost};
use crate::types::OutputItem;

/// Upper bound on VM heap (QuickJS's own allocator enforces it as a catchable
/// allocation failure).
pub(crate) const MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;
/// Guard on QuickJS's own stack: deep recursion becomes a catchable error
/// instead of overflowing the thread.
pub(crate) const STACK_LIMIT_BYTES: usize = 1024 * 1024;
/// How often the result-wait loop steps the interrupt flag.
const RECV_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Everything the VM thread needs for one execution.
pub(crate) struct VmParams {
    pub code: String,
    pub tools_json: String,
    pub store_json: String,
}

/// Run one VM to completion on the current thread. Reports everything through
/// `vm_tx`; returns quietly on interrupt (the host owns the aborted/timeout
/// verdict) and sends a crash message for VM-side breakage.
///
/// Borrow discipline: `Context::with` locks the runtime for the whole closure,
/// so nothing inside a `with` may re-enter the runtime. The setup (natives,
/// prelude, script start) runs in one `with`; the drive loop runs outside it
/// and re-enters `with` briefly for each JS interaction, with the prelude API
/// object kept alive across those windows by `Persistent`.
pub(crate) fn run_vm(
    params: VmParams,
    vm_tx: tokio::sync::mpsc::UnboundedSender<VmToHost>,
    host_rx: std::sync::mpsc::Receiver<HostToVm>,
    interrupt: Arc<AtomicBool>,
) {
    let Ok(runtime) = rquickjs::Runtime::new() else {
        let _ = vm_tx.send(VmToHost::Crash(
            "the QuickJS runtime could not be created".into(),
        ));
        return;
    };
    runtime.set_memory_limit(MEMORY_LIMIT_BYTES);
    runtime.set_max_stack_size(STACK_LIMIT_BYTES);
    let flag = Arc::clone(&interrupt);
    runtime.set_interrupt_handler(Some(Box::new(move || flag.load(Ordering::Relaxed))));
    let Ok(context) = Context::full(&runtime) else {
        let _ = vm_tx.send(VmToHost::Crash(
            "the QuickJS context could not be created".into(),
        ));
        return;
    };
    // Dropped before the runtime at scope exit, as Persistent requires.
    let Ok((api, done)) = context.with(|ctx| setup(&ctx, &params, &vm_tx, &interrupt)) else {
        return; // setup reported the outcome itself
    };
    drive(&runtime, &context, api, done, &vm_tx, &host_rx, &interrupt);
}

/// Install the bridge natives, evaluate the prelude, evaluate the script, and
/// start it. Everything the host needs afterwards hangs off the returned API
/// object (`run`, `settle`, `pendingCount`), kept alive by `Persistent`.
/// `Err(())` = the outcome was already reported (bridge install failure,
/// compile error, uncatchable start failure) or the host owns the verdict.
fn setup(
    ctx: &rquickjs::Ctx<'_>,
    params: &VmParams,
    vm_tx: &tokio::sync::mpsc::UnboundedSender<VmToHost>,
    interrupt: &Arc<AtomicBool>,
) -> Result<(Persistent<Object<'static>>, Arc<AtomicBool>), ()> {
    let done = Arc::new(AtomicBool::new(false));

    let call_tx = vm_tx.clone();
    let host_call = Function::new(
        ctx.clone(),
        move |id: u32, name: String, args_json: String| {
            // The promise the script awaits was already created on the JS side;
            // this only hands the call to the host and returns immediately, so
            // `Promise.all` stays concurrent.
            let _ = call_tx.send(VmToHost::Call {
                call_id: u64::from(id),
                name,
                args_json,
            });
        },
    )
    .map_err(|_| {
        let _ = vm_tx.send(VmToHost::Crash(
            "the host call bridge could not be installed".into(),
        ));
    })?;

    let output_tx = vm_tx.clone();
    let host_output = Function::new(
        ctx.clone(),
        move |kind: String, data: String, mime_type: Option<String>| {
            let item = match kind.as_str() {
                "image" => OutputItem::Image {
                    data,
                    mime_type: mime_type.unwrap_or_else(|| "application/octet-stream".into()),
                },
                "console" => OutputItem::Text {
                    text: data,
                    console: true,
                },
                _ => OutputItem::Text {
                    text: data,
                    console: false,
                },
            };
            let _ = output_tx.send(VmToHost::Output(item));
        },
    )
    .map_err(|_| {
        let _ = vm_tx.send(VmToHost::Crash(
            "the output bridge could not be installed".into(),
        ));
    })?;

    let done_tx = vm_tx.clone();
    let done_flag = Arc::clone(&done);
    let host_done = Function::new(
        ctx.clone(),
        move |ok: bool, payload: Option<String>, writes: Option<String>| {
            let message = if ok {
                DoneMessage::Ok {
                    value_json: payload,
                    writes_json: writes.unwrap_or_else(|| "[]".into()),
                }
            } else {
                DoneMessage::Err {
                    error_json: payload
                        .unwrap_or_else(|| "{\"message\":\"the script failed\"}".to_owned()),
                }
            };
            done_flag.store(true, Ordering::Relaxed);
            let _ = done_tx.send(VmToHost::Done(message));
        },
    )
    .map_err(|_| {
        let _ = vm_tx.send(VmToHost::Crash(
            "the done bridge could not be installed".into(),
        ));
    })?;

    let mut prelude_options = EvalOptions::default();
    prelude_options.filename = Some("manox-codemode-prelude.js".into());
    let factory: Function = match ctx.eval_with_options(crate::PRELUDE, prelude_options) {
        Ok(factory) => factory,
        Err(_) => {
            let error_json = describe_exception(ctx);
            let _ = vm_tx.send(VmToHost::Crash(format!(
                "the sandbox prelude failed to evaluate: {error_json}"
            )));
            return Err(());
        }
    };
    let api: Object = match factory.call((
        host_call,
        host_output,
        host_done,
        params.tools_json.as_str(),
        params.store_json.as_str(),
    )) {
        Ok(api) => api,
        Err(_) => {
            let _ = vm_tx.send(VmToHost::Crash(
                "the sandbox prelude failed to initialize".into(),
            ));
            return Err(());
        }
    };

    // The wrapper prefix stays on one line so script line numbers in error
    // stacks match the source as written (the @options line was blanked).
    let wrapper = format!("(async (tools, console) => {{{}\n}})", params.code);
    let mut script_options = EvalOptions::default();
    script_options.filename = Some("codemode.js".into());
    let script_fn: Function = match ctx.eval_with_options(wrapper, script_options) {
        Ok(fn_value) => fn_value,
        Err(_) => {
            let error_json = describe_exception(ctx);
            let _ = vm_tx.send(VmToHost::Done(DoneMessage::Err { error_json }));
            return Err(());
        }
    };

    let run: Function = match api.get("run") {
        Ok(run) => run,
        Err(_) => {
            let _ = vm_tx.send(VmToHost::Crash(
                "the sandbox prelude is missing its run hook".into(),
            ));
            return Err(());
        }
    };
    // Uncatchable errors (interrupt) surface here; script-level throws are
    // handled inside the prelude and arrive as a Done message.
    if run.call::<_, Value>((script_fn,)).is_err() {
        if !interrupt.load(Ordering::Relaxed) {
            let _ = vm_tx.send(VmToHost::Crash("the script could not be started".into()));
        }
        return Err(());
    }
    Ok((Persistent::save(ctx, api), done))
}

/// Drive the started VM: drain the promise job queue, fail hung scripts,
/// deliver tool results, and exit on the interrupt flag. JS interactions go
/// through short `with` re-entries; the queue is drained outside them.
fn drive(
    runtime: &rquickjs::Runtime,
    context: &Context,
    api: Persistent<Object<'static>>,
    done: Arc<AtomicBool>,
    vm_tx: &tokio::sync::mpsc::UnboundedSender<VmToHost>,
    host_rx: &std::sync::mpsc::Receiver<HostToVm>,
    interrupt: &Arc<AtomicBool>,
) {
    loop {
        // Drain the promise job queue, checking the interrupt flag between
        // jobs: a loop like `while (true) await null` is one job per
        // iteration, and QuickJS's own opcode-counted polling alone would let
        // it run long between polls.
        loop {
            match runtime.execute_pending_job() {
                Ok(true) => {
                    if interrupt.load(Ordering::Relaxed) {
                        return;
                    }
                }
                Ok(false) => break,
                Err(job_exception) => {
                    if interrupt.load(Ordering::Relaxed) {
                        return;
                    }
                    let described = job_exception.0.with(|ctx| describe_exception(&ctx));
                    let _ = vm_tx.send(VmToHost::Crash(format!(
                        "a promise job failed: {described}"
                    )));
                    return;
                }
            }
        }
        if done.load(Ordering::Relaxed) || interrupt.load(Ordering::Relaxed) {
            return;
        }
        let pending_count: Option<u32> = context.with(|ctx| {
            let api = api.clone().restore(&ctx).ok()?;
            let hook: Function = api.get("pendingCount").ok()?;
            hook.call::<_, u32>(()).ok()
        });
        let Some(pending_count) = pending_count else {
            let _ = vm_tx.send(VmToHost::Crash(
                "the sandbox prelude is missing its pending-count hook".into(),
            ));
            return;
        };
        if pending_count == 0 {
            // The script awaits a promise nothing can ever settle: no tool
            // call is pending, and timers do not exist in the VM.
            let _ = vm_tx.send(VmToHost::Done(DoneMessage::Err {
                error_json: "{\"name\":\"Error\",\"message\":\"The script is waiting on a promise that can never \
                             settle: no tool call is pending, and timers do not exist here.\"}"
                    .into(),
            }));
            return;
        }
        // The script is suspended waiting for tool results: deliver them as
        // they arrive (out of order is fine — ids pair them), stepping the
        // interrupt flag so cancellation reaches a waiting VM too.
        loop {
            match host_rx.recv_timeout(RECV_POLL_INTERVAL) {
                Ok(HostToVm::Result {
                    call_id,
                    ok,
                    payload,
                }) => {
                    let delivered = context.with(|ctx| {
                        let api = api
                            .clone()
                            .restore(&ctx)
                            .map_err(|error| error.to_string())?;
                        let settle: Function = api
                            .get("settle")
                            .map_err(|_| "missing settle hook".to_owned())?;
                        // The resolved value stays inside the VM; only the
                        // outcome escapes the `with` window.
                        settle
                            .call::<_, Value>((
                                u32::try_from(call_id).unwrap_or(u32::MAX),
                                ok,
                                payload,
                            ))
                            .map(|_| ())
                            .map_err(|_| "settle failed".to_owned())
                    });
                    if let Err(delivery_error) = delivered
                        && !interrupt.load(Ordering::Relaxed)
                    {
                        let _ = vm_tx.send(VmToHost::Crash(format!(
                            "a tool result could not be delivered: {delivery_error}"
                        )));
                        return;
                    }
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {
                    if interrupt.load(Ordering::Relaxed) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// Describe the pending exception on `ctx` as the prelude's error JSON shape
/// (`{"name": ..., "message": ..., "stack": ...}` or `{"message": ...}`).
/// `Ctx::catch` clears the exception.
fn describe_exception(ctx: &rquickjs::Ctx<'_>) -> String {
    let value = ctx.catch();
    if value.is_undefined() {
        return "{\"message\":\"an unknown error occurred\"}".to_owned();
    }
    let property = |name: &str| -> Option<String> {
        let field: Option<Coerced<String>> = value.as_object()?.get(name).ok()?;
        field.map(|coerced| coerced.0)
    };
    let name = property("name");
    let message = property("message");
    let stack = property("stack");
    let mut object = serde_json::Map::new();
    if let Some(name) = name {
        object.insert("name".into(), serde_json::Value::String(name));
    }
    object.insert(
        "message".into(),
        serde_json::Value::String(message.unwrap_or_else(|| "the script threw an error".into())),
    );
    if let Some(stack) = stack {
        object.insert("stack".into(), serde_json::Value::String(stack));
    }
    serde_json::Value::Object(object).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_json_shape_round_trips_through_host_parsing() {
        let json = describe_exception_shape();
        assert!(json.contains("\"message\""));
    }

    fn describe_exception_shape() -> String {
        let runtime = rquickjs::Runtime::new().expect("runtime");
        let context = Context::full(&runtime).expect("context");
        context.with(|ctx| {
            let _ = ctx.eval::<Value, _>("throw new TypeError('boom')");
            describe_exception(&ctx)
        })
    }
}
