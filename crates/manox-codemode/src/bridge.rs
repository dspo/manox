//! The promise bridge's host half: message protocol, the host-side relay loop,
//! concurrent nested tool execution, and call records.
//!
//! Contract (the VM half lives in [`crate::sandbox`] and `prelude.js`):
//! - `Call` messages arrive synchronously in script order; each is answered
//!   with a `Result` message carrying the same `call_id`. The native side never
//!   blocks: results return whenever they return, so `Promise.all` in the
//!   script stays genuinely concurrent.
//! - Nested calls execute concurrently on the host (`tokio::spawn` per call),
//!   each with a derived cancellation token. The global in-flight cap rejects
//!   overflow calls immediately rather than queueing them.
//! - The first terminal wins: `Done`, `Crash`, the deadline, or the caller's
//!   cancellation. A caller cancellation overrides a `Done::Err` that raced
//!   it — the rejection the script reported was the cancellation itself, so
//!   the verdict must be `Aborted`, not a scheduling-dependent script error.
//!   Every terminal cancels the in-flight calls and raises the VM interrupt
//!   flag; records of calls that never completed stay `cancelled`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::CodemodeTool;
use crate::sandbox::VmParams;
use crate::types::{CallRecord, CallStatus, MAX_IN_FLIGHT_CALLS, OutputItem};

/// VM -> host traffic.
pub(crate) enum VmToHost {
    Call {
        call_id: u64,
        name: String,
        args_json: String,
    },
    Output(OutputItem),
    Done(DoneMessage),
    /// The VM broke outside the script: prelude failure, undeliverable result,
    /// a job that failed without an interrupt.
    Crash(String),
}

/// The script's verdict, reported exactly once by the prelude.
pub(crate) enum DoneMessage {
    Ok {
        value_json: Option<String>,
        writes_json: String,
    },
    Err {
        error_json: String,
    },
}

/// Host -> VM traffic.
pub(crate) enum HostToVm {
    Result {
        call_id: u64,
        ok: bool,
        payload: String,
    },
}

/// How the execution ended.
pub(crate) enum Terminal {
    /// The prelude reported the script's own outcome.
    Done(DoneMessage),
    /// The `@options` deadline fired.
    Timeout,
    /// The caller's cancellation token fired.
    Aborted,
    /// The sandbox or the bridge broke.
    Sandbox(String),
}

/// What the host collected for one execution.
pub(crate) struct ExecutionOutcome {
    pub output: Vec<OutputItem>,
    pub calls: Vec<CallRecord>,
    pub terminal: Terminal,
}

/// Drive one execution: spawn the VM thread, relay calls to the tools, and
/// return what the script produced. Requires a tokio runtime context (nested
/// calls are spawned onto it).
pub(crate) async fn run(
    params: VmParams,
    tools: &HashMap<String, Arc<dyn CodemodeTool>>,
    deadline: Option<Duration>,
    cancel: CancellationToken,
) -> ExecutionOutcome {
    let empty = || ExecutionOutcome {
        output: Vec::new(),
        calls: Vec::new(),
        terminal: Terminal::Aborted,
    };
    if cancel.is_cancelled() {
        return empty();
    }
    let internal = cancel.child_token();
    let call_root = internal.child_token();
    let interrupt = Arc::new(AtomicBool::new(false));
    let (vm_tx, mut vm_rx) = tokio::sync::mpsc::unbounded_channel::<VmToHost>();
    let (result_tx, host_rx) = std::sync::mpsc::channel::<HostToVm>();

    let thread_interrupt = Arc::clone(&interrupt);
    let thread_handle = std::thread::Builder::new()
        .name("manox-codemode-vm".into())
        .spawn(move || crate::sandbox::run_vm(params, vm_tx.clone(), host_rx, thread_interrupt))
        .map_err(|error| format!("the codemode VM thread could not be spawned: {error}"));
    let thread_handle = match thread_handle {
        Ok(handle) => handle,
        Err(message) => {
            return ExecutionOutcome {
                output: Vec::new(),
                calls: Vec::new(),
                terminal: Terminal::Sandbox(message),
            };
        }
    };

    // Caller cancellation reaches the VM through the interrupt flag.
    {
        let internal = internal.clone();
        let interrupt = Arc::clone(&interrupt);
        tokio::spawn(async move {
            internal.cancelled().await;
            interrupt.store(true, Ordering::Relaxed);
        });
    }

    let output = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let deadline_at = deadline.map(|budget| tokio::time::Instant::now() + budget);
    let mut deadline_sleep =
        Box::pin(tokio::time::sleep_until(deadline_at.unwrap_or_else(|| {
            tokio::time::Instant::now() + Duration::from_secs(365 * 24 * 3600)
        })));
    let mut terminal: Option<Terminal> = None;

    let dispatch = CallDispatch {
        calls: &calls,
        in_flight: &in_flight,
        call_root: &call_root,
        result_tx: &result_tx,
        tools,
    };
    while terminal.is_none() {
        tokio::select! {
            message = vm_rx.recv() => match message {
                Some(VmToHost::Call { call_id, name, args_json }) => {
                    dispatch.handle(call_id, name, args_json);
                }
                Some(VmToHost::Output(item)) => output.lock().expect("output lock").push(item),
                Some(VmToHost::Done(message)) => terminal = Some(Terminal::Done(message)),
                Some(VmToHost::Crash(message)) => terminal = Some(Terminal::Sandbox(message)),
                // The VM thread died without reporting (panic, or a crash send
                // that raced the channel close).
                None => {
                    terminal = Some(Terminal::Sandbox("the codemode VM thread exited without reporting a result".into()))
                }
            },
            _ = &mut deadline_sleep, if deadline_at.is_some() => {
                terminal = Some(Terminal::Timeout);
            }
            _ = cancel.cancelled() => {
                terminal = Some(Terminal::Aborted);
            }
        }
    }

    // The caller's cancellation is the verdict, not the failure it causes: a
    // cancelled nested call rejects the promise, so a script can report its
    // own "failed" done before the relay loop observes the cancel. Both
    // orders mean the user stopped the run, and the result must say so —
    // otherwise the outcome depends on a scheduling race. A run that
    // completed (`Done::Ok`) keeps its own verdict, and the deadline race is
    // unchanged (first terminal wins).
    let terminal = match terminal.expect("terminal set above") {
        Terminal::Done(DoneMessage::Err { .. }) if cancel.is_cancelled() => Terminal::Aborted,
        terminal => terminal,
    };

    // First terminal closes the execution: raise the interrupt flag so the VM
    // thread unwinds, and cancel every in-flight call via its derived token.
    interrupt.store(true, Ordering::Relaxed);
    internal.cancel();
    drop(result_tx);
    // The VM thread owns its Runtime and exits promptly once it sees the flag
    // (interrupt polls in JS, `recv_timeout` steps in the wait loop). Join
    // politely; a thread that misses the deadline detaches and cleans up on
    // its own — nothing outside the thread ever touches the Runtime.
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::task::spawn_blocking(move || thread_handle.join()),
    )
    .await;

    ExecutionOutcome {
        output: std::mem::take(&mut output.lock().expect("output lock")),
        calls: std::mem::take(&mut calls.lock().expect("calls lock")),
        terminal,
    }
}

/// Shared state the call dispatcher needs. Bundled so one execution's relay
/// loop and its spawned tasks see the same view.
struct CallDispatch<'a> {
    calls: &'a Arc<Mutex<Vec<CallRecord>>>,
    in_flight: &'a Arc<AtomicUsize>,
    call_root: &'a CancellationToken,
    result_tx: &'a std::sync::mpsc::Sender<HostToVm>,
    tools: &'a HashMap<String, Arc<dyn CodemodeTool>>,
}

impl CallDispatch<'_> {
    /// Record a call and dispatch it. `calls` is pre-seeded with a `cancelled`
    /// record (keyed by position) so a call still in flight at the terminal is
    /// reported as cancelled by construction; the dispatch task overwrites its
    /// slot when the tool settles.
    fn handle(&self, call_id: u64, name: String, args_json: String) {
        let record_slot = {
            let mut records = self.calls.lock().expect("calls lock");
            records.push(CallRecord {
                name: name.clone(),
                status: CallStatus::Cancelled,
                duration_ms: 0,
                error: None,
            });
            records.len() - 1
        };
        let reject = |slot: usize, message: String| {
            set_record(
                self.calls,
                slot,
                CallRecord {
                    name: name.clone(),
                    status: CallStatus::Error,
                    duration_ms: 0,
                    error: Some(message.clone()),
                },
            );
            let _ = self.result_tx.send(HostToVm::Result {
                call_id,
                ok: false,
                payload: message,
            });
        };

        // Defensive re-validation: the VM side rejects unknown names too, but
        // the host never trusts bridge traffic.
        let Some(tool) = self.tools.get(&name).cloned() else {
            reject(record_slot, format!("unknown tool \"{name}\""));
            return;
        };
        if self.in_flight.load(Ordering::SeqCst) >= MAX_IN_FLIGHT_CALLS {
            reject(
                record_slot,
                format!(
                    "too many concurrent tool calls: the limit is {MAX_IN_FLIGHT_CALLS}. Await some calls before issuing more."
                ),
            );
            return;
        }
        let args = match serde_json::from_str::<serde_json::Value>(&args_json) {
            Ok(args) => args,
            Err(error) => {
                reject(
                    record_slot,
                    format!("the call arguments are not valid JSON: {error}"),
                );
                return;
            }
        };

        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let token = self.call_root.child_token();
        let in_flight = Arc::clone(self.in_flight);
        let calls = Arc::clone(self.calls);
        let result_tx = self.result_tx.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            let result = tool
                .execute(&call_id.to_string(), args, token.clone())
                .await;
            let cancelled = token.is_cancelled();
            let (status, ok, payload, error) = match result {
                Ok(value) => (CallStatus::Ok, true, value.to_string(), None),
                Err(_text) if cancelled => (
                    CallStatus::Cancelled,
                    false,
                    "tool call cancelled".to_owned(),
                    None,
                ),
                Err(text) => (CallStatus::Error, false, text.clone(), Some(text)),
            };
            let record = CallRecord {
                name,
                status,
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                error,
            };
            set_record(&calls, record_slot, record);
            let _ = result_tx.send(HostToVm::Result {
                call_id,
                ok,
                payload,
            });
            in_flight.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// Overwrite the record at `slot`; the slot was pre-registered for this call,
/// so a finished terminal dropping the outcome race is harmless (nothing reads
/// the vec after the terminal).
fn set_record(calls: &Mutex<Vec<CallRecord>>, slot: usize, record: CallRecord) {
    let mut records = calls.lock().expect("calls lock");
    if let Some(existing) = records.get_mut(slot) {
        *existing = record;
    }
}
