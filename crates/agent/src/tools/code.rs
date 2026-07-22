//! Restricted QuickJS orchestration tool used by hybrid code mode.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use gpui::{App, Task};
use rquickjs::context::EvalOptions;
use rquickjs::function::{Async, Func};
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Promise};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

pub const MAX_NESTED_CALLS: usize = 32;
pub const MAX_CONCURRENCY: usize = 8;
const MEMORY_LIMIT: usize = 32 * 1024 * 1024;
const STACK_LIMIT: usize = 512 * 1024;
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

pub const ALLOWED_TOOLS: &[&str] = &[
    super::READ,
    super::GREP,
    super::GLOB,
    super::LIST,
    super::BASH,
    super::BASH_OUTPUT,
    super::EDIT,
    super::WRITE,
    super::WEB_FETCH,
    "LspStatus",
    "LspWaitReady",
    "DocumentSymbols",
    "WorkspaceSymbols",
    "GoToDefinition",
    "FindReferences",
    "Hover",
    "Diagnostics",
];

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodeInput {
    /// Restricted JavaScript body. Call tools with
    /// `await tools.Read({...})`; expose only selected data with `text(value)`
    /// or an explicit return value. `Promise.all` is supported.
    pub(crate) script: String,
}

pub struct CodeTool;

impl AgentTool for CodeTool {
    fn name(&self) -> &str {
        super::CODE
    }

    fn description(&self) -> &str {
        "Run restricted JavaScript to orchestrate allowed native tools and project only selected results. Use await tools.<Name>(input), Promise.all, text(value), or return. No filesystem, network, process, environment, import, eval, or nested Code access is available except through the declared tools."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema::<CodeInput>()
    }

    fn is_read_only(&self) -> bool {
        // Plan mode keeps Code visible; every nested call is independently
        // checked by the execution backstop.
        true
    }

    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn run(
        &self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        _cx: &mut App,
    ) -> Task<Result<String, String>> {
        Task::ready(Err(
            "Code must run through the thread nested-tool dispatcher".to_string(),
        ))
    }
}

#[derive(Debug)]
pub struct NestedRequest {
    pub sequence: usize,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub response: tokio::sync::oneshot::Sender<NestedResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NestedResponse {
    pub ok: bool,
    pub output: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScriptOutcome {
    #[serde(default)]
    pub selected: Vec<String>,
    pub returned: Option<String>,
    pub calls: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub sequence: usize,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub ok: bool,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeEnvelope {
    pub version: u32,
    #[serde(default)]
    pub selected: Vec<String>,
    pub returned: Option<String>,
    pub error: Option<String>,
    pub calls: usize,
    #[serde(default)]
    pub audit: Vec<AuditEntry>,
}

pub fn envelope_json(outcome: Result<ScriptOutcome, String>, mut audit: Vec<AuditEntry>) -> String {
    audit.sort_by_key(|entry| entry.sequence);
    let (selected, returned, error, calls) = match outcome {
        Ok(outcome) => (outcome.selected, outcome.returned, None, outcome.calls),
        Err(error) => (Vec::new(), None, Some(error), audit.len()),
    };
    serde_json::to_string(&CodeEnvelope {
        version: 1,
        selected,
        returned,
        error,
        calls,
        audit,
    })
    .unwrap_or_else(|error| format!("Code envelope serialization failed: {error}"))
}

pub fn model_text(raw: &str) -> String {
    let Ok(envelope) = serde_json::from_str::<CodeEnvelope>(raw) else {
        return raw.to_string();
    };
    let rendered = if let Some(error) = envelope.error {
        format!("Code failed: {error}")
    } else {
        let mut output = envelope.selected;
        if let Some(returned) = envelope.returned {
            output.push(returned);
        }
        if output.is_empty() {
            format!(
                "Code completed {} nested call(s); no result was selected with text() or return.",
                envelope.calls
            )
        } else {
            output.join("\n")
        }
    };
    crate::optimizer::compact_tool_output(
        super::CODE,
        &rendered,
        crate::optimizer::tool_budget(super::CODE),
    )
}

/// Execute the isolate on the Tokio runtime and return a receiver for its final
/// result. Native calls are sent back to the foreground dispatcher.
pub fn spawn_script(
    script: String,
    requests: async_channel::Sender<NestedRequest>,
    cancel: CancellationToken,
) -> async_channel::Receiver<Result<ScriptOutcome, String>> {
    let (done_tx, done_rx) = async_channel::bounded(1);
    // QuickJS runtimes are deliberately !Send. Keep each isolate on its own
    // current-thread executor instead of weakening that safety invariant.
    let thread_tx = done_tx.clone();
    let spawned = std::thread::Builder::new()
        .name("manox-code-isolate".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .map_err(|error| format!("Code executor initialization failed: {error}"))
                .and_then(|runtime| runtime.block_on(run_script(script, requests, cancel)));
            let _ = thread_tx.send_blocking(result);
        });
    if let Err(error) = spawned {
        let _ = done_tx.try_send(Err(format!("Code isolate could not start: {error}")));
    }
    done_rx
}

async fn run_script(
    script: String,
    requests: async_channel::Sender<NestedRequest>,
    cancel: CancellationToken,
) -> Result<ScriptOutcome, String> {
    let runtime = AsyncRuntime::new().map_err(js_error)?;
    runtime.set_memory_limit(MEMORY_LIMIT).await;
    runtime.set_max_stack_size(STACK_LIMIT).await;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupt_flag = interrupted.clone();
    let cancel_for_interrupt = cancel.clone();
    let started = Instant::now();
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            let stop = cancel_for_interrupt.is_cancelled() || started.elapsed() > SCRIPT_TIMEOUT;
            if stop {
                interrupt_flag.store(true, Ordering::Release);
            }
            stop
        })))
        .await;
    let context = AsyncContext::full(&runtime).await.map_err(js_error)?;
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_for_call = counter.clone();

    let result = context
        .async_with(async |ctx| {
            let requests = requests.clone();
            let cancel = cancel.clone();
            let host_call = move |name: String, input: String| {
                let requests = requests.clone();
                let cancel = cancel.clone();
                let counter = counter_for_call.clone();
                async move {
                    let sequence = counter.fetch_add(1, Ordering::AcqRel);
                    if sequence >= MAX_NESTED_CALLS {
                        return serde_json::to_string(&NestedResponse {
                            ok: false,
                            output: format!("nested call limit exceeded ({MAX_NESTED_CALLS})"),
                        })
                        .unwrap();
                    }
                    if !ALLOWED_TOOLS.contains(&name.as_str()) {
                        return serde_json::to_string(&NestedResponse {
                            ok: false,
                            output: format!("tool not allowed in Code: {name}"),
                        })
                        .unwrap();
                    }
                    let input = match serde_json::from_str(&input) {
                        Ok(input) => input,
                        Err(error) => {
                            return serde_json::to_string(&NestedResponse {
                                ok: false,
                                output: format!("invalid tool input: {error}"),
                            })
                            .unwrap();
                        }
                    };
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let request = NestedRequest {
                        sequence,
                        tool_name: name,
                        input,
                        response: tx,
                    };
                    let response = tokio::select! {
                        sent = requests.send(request) => {
                            if sent.is_err() {
                                NestedResponse { ok: false, output: "Code dispatcher stopped".into() }
                            } else {
                                rx.await.unwrap_or(NestedResponse { ok: false, output: "nested tool cancelled".into() })
                            }
                        }
                        _ = cancel.cancelled() => NestedResponse { ok: false, output: "Code cancelled".into() },
                    };
                    serde_json::to_string(&response).unwrap()
                }
            };
            ctx.globals()
                .set("__hostCall", Func::from(Async(host_call)))
                .catch(&ctx)
                .map_err(|error| format!("QuickJS: {error}"))?;

            let tool_bindings = ALLOWED_TOOLS
                .iter()
                .map(|name| {
                    format!(
                        "{name}: async (input) => {{ const r = JSON.parse(await __hostCall({quoted}, JSON.stringify(input ?? {{}}))); if (!r.ok) throw new Error(r.output); return r.output; }}",
                        quoted = serde_json::to_string(name).unwrap()
                    )
                })
                .collect::<Vec<_>>()
                .join(",\n");
            let source = format!(
                r#"
                const AsyncFunction = (async () => {{}}).constructor;
                const GeneratorFunction = (function* () {{}}).constructor;
                const AsyncGeneratorFunction = (async function* () {{}}).constructor;
                for (const constructor of [
                    Function, AsyncFunction, GeneratorFunction, AsyncGeneratorFunction
                ]) {{
                    Object.defineProperty(constructor.prototype, "constructor", {{
                        value: undefined, writable: false, configurable: false
                    }});
                }}
                globalThis.process = undefined;
                globalThis.require = undefined;
                globalThis.fetch = undefined;
                globalThis.XMLHttpRequest = undefined;
                globalThis.WebSocket = undefined;
                globalThis.eval = undefined;
                globalThis.Function = undefined;
                const tools = Object.freeze({{ {tool_bindings} }});
                (async () => {{
                    const selected = [];
                    globalThis.text = (value) => {{
                        if (typeof value === "string") selected.push(value);
                        else selected.push(JSON.stringify(value));
                    }};
                    const value = await (async () => {{ {script}
                    }})();
                    const returned = value === undefined ? null :
                        (typeof value === "string" ? value : JSON.stringify(value));
                    return JSON.stringify({{ selected, returned, calls: {calls_expr} }});
                }})()
                "#,
                calls_expr = "0"
            );
            let mut options = EvalOptions::default();
            options.strict = true;
            let promise: Promise = ctx
                .eval_with_options(source, options)
                .catch(&ctx)
                .map_err(|error| format!("QuickJS: {error}"))?;
            let text: String = promise
                .into_future()
                .await
                .catch(&ctx)
                .map_err(|error| format!("QuickJS: {error}"))?;
            Ok::<_, String>(text)
        })
        .await;

    if interrupted.load(Ordering::Acquire) {
        return Err(if cancel.is_cancelled() {
            "Code cancelled".into()
        } else {
            "Code execution timed out".into()
        });
    }
    let text = result?;
    let mut outcome: ScriptOutcome = serde_json::from_str(&text)
        .map_err(|error| format!("Code result serialization failed: {error}"))?;
    outcome.calls = counter.load(Ordering::Acquire).min(MAX_NESTED_CALLS);
    Ok(outcome)
}

fn js_error(error: impl std::fmt::Display) -> String {
    format!("QuickJS: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn execute(script: &str) -> Result<ScriptOutcome, String> {
        let (request_tx, request_rx) = async_channel::bounded(MAX_CONCURRENCY);
        let done = spawn_script(script.to_string(), request_tx, CancellationToken::new());
        loop {
            tokio::select! {
                result = done.recv() => return result.expect("isolate result"),
                request = request_rx.recv() => {
                    let Ok(request) = request else {
                        return done.recv().await.expect("isolate result");
                    };
                    let output = format!("{}:{}", request.tool_name, request.input);
                    request.response.send(NestedResponse { ok: true, output }).expect("response");
                }
            }
        }
    }

    #[tokio::test]
    async fn projects_only_explicit_text_and_return_value() {
        let outcome = execute(
            r#"
            const raw = await tools.Read({"path":"README.md"});
            text(raw.split(":")[0]);
            return {kept: true};
            "#,
        )
        .await
        .unwrap();
        assert_eq!(outcome.selected, ["Read"]);
        assert_eq!(outcome.returned.as_deref(), Some("{\"kept\":true}"));
        assert_eq!(outcome.calls, 1);
    }

    #[tokio::test]
    async fn dynamic_code_and_host_capabilities_are_absent() {
        let outcome = execute(
            r#"
            text([
                typeof process, typeof require, typeof fetch,
                typeof eval, typeof Function,
                typeof (() => {}).constructor,
                typeof (async () => {}).constructor,
                typeof (function* () {}).constructor,
                typeof (async function* () {}).constructor
            ].join(","));
            "#,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.selected,
            [
                "undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined"
            ]
        );
    }

    #[tokio::test]
    async fn rejects_tools_outside_the_allowlist() {
        let error = execute("await tools.Code({script: 'return 1'});")
            .await
            .unwrap_err();
        assert!(error.contains("not a function") || error.contains("undefined"));
    }

    #[tokio::test]
    async fn enforces_nested_call_limit() {
        let outcome = execute(
            r#"
            let rejected = "";
            for (let i = 0; i < 33; i++) {
                try { await tools.Read({path: String(i)}); }
                catch (error) { rejected = String(error); }
            }
            text(rejected);
            "#,
        )
        .await
        .unwrap();
        assert_eq!(outcome.calls, MAX_NESTED_CALLS);
        assert!(outcome.selected[0].contains("nested call limit exceeded"));
    }

    #[tokio::test]
    async fn promise_all_dispatches_native_calls_concurrently() {
        let (request_tx, request_rx) = async_channel::bounded(MAX_CONCURRENCY);
        let done = spawn_script(
            r#"const values = await Promise.all([
                tools.Read({path: "a"}), tools.Read({path: "b"})
            ]); return values.join(",");"#
                .into(),
            request_tx,
            CancellationToken::new(),
        );
        let first = tokio::time::timeout(Duration::from_secs(1), request_rx.recv())
            .await
            .expect("first request timeout")
            .expect("first request");
        // The second request must arrive before the first is answered; a
        // sequential dispatcher would deadlock here and fail the timeout.
        let second = tokio::time::timeout(Duration::from_secs(1), request_rx.recv())
            .await
            .expect("second request timeout")
            .expect("second request");
        first
            .response
            .send(NestedResponse {
                ok: true,
                output: "A".into(),
            })
            .unwrap();
        second
            .response
            .send(NestedResponse {
                ok: true,
                output: "B".into(),
            })
            .unwrap();
        let outcome = done.recv().await.unwrap().unwrap();
        assert_eq!(outcome.calls, 2);
        assert_eq!(outcome.returned.as_deref(), Some("A,B"));
    }

    #[tokio::test]
    async fn cancellation_interrupts_cpu_loop() {
        let (request_tx, _request_rx) = async_channel::bounded(MAX_CONCURRENCY);
        let cancel = CancellationToken::new();
        let done = spawn_script("while (true) {}".into(), request_tx, cancel.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), done.recv())
            .await
            .expect("interrupt timeout")
            .expect("isolate result");
        assert_eq!(result.unwrap_err(), "Code cancelled");
    }

    #[test]
    fn model_projection_obeys_default_tool_budget() {
        let raw = envelope_json(
            Ok(ScriptOutcome {
                selected: vec!["x".repeat(64 * 1024)],
                returned: None,
                calls: 0,
            }),
            Vec::new(),
        );
        assert!(model_text(&raw).len() <= crate::optimizer::tool_budget(super::super::CODE));
    }
}
