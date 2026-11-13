//! manox-codemode —— QuickJS script sandbox with a tool bridge.
//!
//! The model writes a JS script (an async function body) that runs in a
//! QuickJS VM. The script's only capability that leaves the VM is
//! `tools.<name>(args)`: calls into the tools injected at construction. Only
//! the script's output and `return` value reach the caller — the raw results
//! of nested tool calls never enter the model's context, which is the point of
//! this crate (parallel fan-out plus filter/aggregation over large results,
//! with zero new authority: nested calls inherit whatever approval wrapper the
//! injected tool instances already carry).
//!
//! Layout: [`types`] (result surface), [`source`] (`@options` parsing),
//! [`declarations`] (JSON Schema -> TS declarations), [`store`] (store
//! snapshot and writes), [`bridge`] (host half of the promise bridge),
//! [`sandbox`] (the VM thread). `prelude.js` is the in-VM environment
//! contract.
//!
//! `CodemodeExecutor::execute` requires a tokio runtime context: nested tool
//! calls are spawned onto it.

mod bridge;
pub mod declarations;
mod sandbox;
pub mod source;
pub mod store;
pub mod types;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::bridge::{DoneMessage, ExecutionOutcome, Terminal};
use crate::declarations::{DeclaredTool, RenderedDeclarations};
use crate::source::ParsedSource;
use crate::store::StoreSnapshot;

pub use crate::types::{
    CallRecord, CallStatus, CodemodeError, CodemodeErrorKind, CodemodeResult, OutputItem,
    StoreWrites,
};

/// The prelude evaluated inside every VM: the environment contract.
pub(crate) const PRELUDE: &str = include_str!("prelude.js");

/// Process-wide cap on concurrently running executions. One process hosts many
/// sessions (windows), and each VM can reserve its memory budget — the slot
/// cap bounds how far those reservations can stack. Arrival order queues.
pub const MAX_CONCURRENT_EXECUTIONS: usize = 2;
/// How long an execution may wait in line for a process-wide slot before it
/// fails without running.
pub const EXECUTION_QUEUE_TIMEOUT: Duration = Duration::from_secs(60);

/// The process-wide execution slots. One semaphore per process (tests can
/// hold permits directly to pin queueing behavior).
pub fn execution_slots() -> &'static Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_EXECUTIONS)))
}

/// Acquire one process-wide execution slot, queueing in arrival order up to
/// `timeout`. A timeout is a host-resource failure, not a script error, so it
/// classifies as [`CodemodeErrorKind::Sandbox`].
pub async fn acquire_execution_slot(
    timeout: Duration,
) -> Result<tokio::sync::OwnedSemaphorePermit, CodemodeError> {
    let acquire = Semaphore::acquire_owned(Arc::clone(execution_slots()));
    match tokio::time::timeout(timeout, acquire).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(CodemodeError::sandbox(
            "the codemode execution slot semaphore is closed; this is a runtime bug",
        )),
        Err(_) => Err(CodemodeError::sandbox(format!(
            "another codemode execution holds the process-wide slot; waited {}s",
            timeout.as_secs()
        ))),
    }
}

/// A tool a codemode script can call via `tools.<name>(args)`. The crate has
/// no harness dependency: the caller adapts its own tool type to this surface.
/// Failure is a plain error string — its text becomes the JS rejection the
/// script sees.
#[async_trait::async_trait]
pub trait CodemodeTool: Send + Sync {
    /// Tool name as the bridge exposes it: `tools["<name>"]` always works, and
    /// `tools.<js_name>` is the identifier-normalized spelling.
    fn name(&self) -> &str;
    /// One-line description; feeds `ALL_TOOLS` and `describeTool`.
    fn description(&self) -> &str;
    /// JSON Schema of the single `args` object; rendered into the tool's
    /// TypeScript declaration.
    fn parameters_schema(&self) -> serde_json::Value;
    /// Execute one nested call. `signal` fires when the execution ends before
    /// the call did (timeout, abort, or the script finishing early): stop work
    /// and return promptly.
    async fn execute(
        &self,
        call_id: &str,
        args: serde_json::Value,
        signal: CancellationToken,
    ) -> Result<serde_json::Value, String>;
}

/// Maps a tool name to the identifier scripts use for `tools.<name>`: illegal
/// identifier characters become `_` (`mcp__a-b` -> `mcp__a_b`). The empty name
/// maps to `_`.
pub fn to_js_identifier(name: &str) -> String {
    let mut identifier = String::with_capacity(name.len());
    for (index, character) in name.chars().enumerate() {
        let valid = if index == 0 {
            character.is_ascii_alphabetic() || character == '_' || character == '$'
        } else {
            character.is_ascii_alphanumeric() || character == '_' || character == '$'
        };
        identifier.push(if valid { character } else { '_' });
    }
    if identifier.is_empty() {
        return "_".to_owned();
    }
    identifier
}

/// Ready-to-run codemode executor: injects the tools a script may call and
/// renders their declarations once, at construction.
pub struct CodemodeExecutor {
    tools: HashMap<String, Arc<dyn CodemodeTool>>,
    /// Tool list in construction order with rendered declarations.
    declared: Vec<DeclaredTool>,
    rendered: RenderedDeclarations,
}

impl CodemodeExecutor {
    /// Build an executor over the given tools. Rejects duplicate names and
    /// names that normalize to the same script identifier — both would make
    /// `tools.<name>` ambiguous.
    pub fn new(tools: Vec<Arc<dyn CodemodeTool>>) -> Result<Self, String> {
        let mut map: HashMap<String, Arc<dyn CodemodeTool>> = HashMap::new();
        let mut declared = Vec::with_capacity(tools.len());
        let mut seen_identifiers = std::collections::HashSet::new();
        for tool in tools {
            let name = tool.name().to_owned();
            if map.contains_key(&name) {
                return Err(format!("duplicate tool name \"{name}\""));
            }
            let js_name = to_js_identifier(&name);
            if !seen_identifiers.insert(js_name.clone()) {
                return Err(format!(
                    "tool name \"{name}\" collides with another tool's script identifier \"{js_name}\""
                ));
            }
            declared.push(DeclaredTool {
                name: name.clone(),
                js_name,
                description: tool.description().to_owned(),
                parameters_schema: tool.parameters_schema(),
            });
            map.insert(name, tool);
        }
        let rendered = declarations::render_declarations(&declared);
        Ok(Self {
            tools: map,
            declared,
            rendered,
        })
    }

    /// Rendered declarations: `rendered` entries carry the per-tool
    /// declaration that passed the budget; `dropped` lists tools whose
    /// declaration exceeded it (the script discovers them via `ALL_TOOLS`).
    pub fn rendered_declarations(&self) -> &RenderedDeclarations {
        &self.rendered
    }

    /// The declared tool list in construction order.
    pub fn declared_tools(&self) -> &[DeclaredTool] {
        &self.declared
    }

    /// Execute one script.
    ///
    /// `source` is the model's JS (optionally with a first-line `// @options:`
    /// directive, see [`source::parse_source`]). `store` is the initial
    /// snapshot (key -> serialized JSON); a successful execution returns its
    /// writes, a failed one returns none. Cancelling `cancel` ends the
    /// execution with [`CodemodeErrorKind::Aborted`].
    ///
    /// Executions are admitted through the process-wide slot cap
    /// ([`MAX_CONCURRENT_EXECUTIONS`]): arrivals queue in order and a wait
    /// past [`EXECUTION_QUEUE_TIMEOUT`] fails as a sandbox-class result. The
    /// slot is held for the whole run and released on completion.
    ///
    /// Requires a tokio runtime context. Never returns `Err`: script failures
    /// are results, and every result carries the output produced before the
    /// failure.
    pub async fn execute(
        &self,
        source: &str,
        store: &StoreSnapshot,
        cancel: CancellationToken,
    ) -> CodemodeResult {
        let started = Instant::now();
        let ParsedSource { code, options } = match source::parse_source(source) {
            Ok(parsed) => parsed,
            Err(error) => return self.empty_result(started, CodemodeError::script(error.message)),
        };
        // The slot acquire is async and must stay on the tokio side, before
        // the VM thread starts (the bridge runs that thread blocking). A
        // caller cancel during the queue wait aborts without running.
        let _slot = tokio::select! {
            slot = acquire_execution_slot(EXECUTION_QUEUE_TIMEOUT) => match slot {
                Ok(permit) => permit,
                Err(error) => return self.empty_result(started, error),
            },
            _ = cancel.cancelled() => {
                return self.empty_result(
                    started,
                    CodemodeError {
                        kind: CodemodeErrorKind::Aborted,
                        message: "Execution aborted".into(),
                        stack: None,
                    },
                );
            }
        };
        let tools_json = self.tools_json();
        let store_json = serde_json::to_string(store).expect("a map of strings always serializes");
        let deadline = options.timeout_ms.map(Duration::from_millis);
        let params = sandbox::VmParams {
            code,
            tools_json,
            store_json,
        };
        let outcome = bridge::run(params, &self.tools, deadline, cancel).await;
        self.assemble(started, &options, outcome)
    }

    fn tools_json(&self) -> String {
        let entries: Vec<serde_json::Value> = self
            .rendered
            .rendered
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "jsName": tool.js_name,
                    "description": tool.description,
                    "declaration": tool.declaration,
                })
            })
            .collect();
        serde_json::to_string(&entries).expect("an array of values always serializes")
    }

    fn empty_result(&self, started: Instant, error: CodemodeError) -> CodemodeResult {
        CodemodeResult {
            output: Vec::new(),
            text: String::new(),
            full_output: None,
            value: None,
            calls: Vec::new(),
            total_calls: 0,
            store_writes: StoreWrites::default(),
            error: Some(error),
            wall_time: started.elapsed(),
        }
    }

    /// Fold the bridge outcome into the result: decode the terminal, render
    /// the output text under the soft limit, cap the call records.
    fn assemble(
        &self,
        started: Instant,
        options: &source::SourceOptions,
        outcome: ExecutionOutcome,
    ) -> CodemodeResult {
        let (mut value, mut writes, mut error) = (None, StoreWrites::default(), None);
        match outcome.terminal {
            Terminal::Done(DoneMessage::Ok {
                value_json,
                writes_json,
            }) => match store::parse_writes(&writes_json) {
                Ok(parsed_writes) => {
                    value = value_json;
                    writes = parsed_writes;
                }
                Err(message) => {
                    error = Some(CodemodeError::sandbox(format!(
                        "the sandbox bridge reported malformed store writes: {message}"
                    )))
                }
            },
            Terminal::Done(DoneMessage::Err { error_json }) => {
                error = Some(script_error_from_json(&error_json));
            }
            Terminal::Timeout => {
                let budget = options.timeout_ms.unwrap_or(0);
                error = Some(CodemodeError {
                    kind: CodemodeErrorKind::Timeout,
                    message: format!("Execution timed out after {budget} ms"),
                    stack: None,
                });
            }
            Terminal::Aborted => {
                error = Some(CodemodeError {
                    kind: CodemodeErrorKind::Aborted,
                    message: "Execution aborted".into(),
                    stack: None,
                });
            }
            Terminal::Sandbox(message) => {
                error = Some(CodemodeError::sandbox(message));
            }
        }
        let (text, full_output) = types::render_output(&outcome.output, options.max_output_tokens);
        // The record list is capped at the crate exit: details.travel into the
        // journal, so a fan-out script must not append unbounded rows. The
        // first records in issue order are kept; the true count travels
        // alongside.
        let total_calls = outcome.calls.len();
        let mut calls = outcome.calls;
        calls.truncate(types::MAX_RECORDED_CALLS);
        CodemodeResult {
            output: outcome.output,
            text,
            full_output,
            value,
            calls,
            total_calls,
            store_writes: writes,
            error,
            wall_time: started.elapsed(),
        }
    }
}

/// Decode the prelude's error JSON (`{"name", "message", "stack"}` or
/// `{"message"}`) into a script error. The name folds into the message so the
/// four-way error kind stays the classification.
fn script_error_from_json(error_json: &str) -> CodemodeError {
    let parsed: serde_json::Value = match serde_json::from_str(error_json) {
        Ok(parsed) => parsed,
        Err(error) => {
            return CodemodeError::sandbox(format!(
                "the sandbox bridge reported a malformed script error: {error}"
            ));
        }
    };
    let name = parsed
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Error");
    let message = parsed
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("the script failed");
    let stack = parsed
        .get("stack")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let message = if name.is_empty() || name == "Error" {
        message.to_owned()
    } else {
        format!("{name}: {message}")
    };
    CodemodeError {
        kind: CodemodeErrorKind::Script,
        message,
        stack,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_identifier_normalization() {
        assert_eq!(to_js_identifier("read"), "read");
        assert_eq!(to_js_identifier("mcp__a-b"), "mcp__a_b");
        assert_eq!(to_js_identifier("9lives"), "_lives");
        assert_eq!(to_js_identifier("my tool.v2"), "my_tool_v2");
        assert_eq!(to_js_identifier(""), "_");
    }

    #[test]
    fn script_error_json_folds_name_into_message() {
        let error =
            script_error_from_json(r#"{"name":"RangeError","message":"too big","stack":"at x"}"#);
        assert_eq!(error.kind, CodemodeErrorKind::Script);
        assert_eq!(error.message, "RangeError: too big");
        assert_eq!(error.stack.as_deref(), Some("at x"));
        let error = script_error_from_json(r#"{"message":"plain"}"#);
        assert_eq!(error.message, "plain");
    }
}
