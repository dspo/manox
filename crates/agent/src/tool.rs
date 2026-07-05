//! Tool abstraction and registry.
//!
//! Built-in tools: read_file / write_file / edit_file / list_directory /
//! bash / grep / glob.
//!
//! Design: an erased `AgentTool` trait (`run` takes a `serde_json::Value` input
//! and returns a `Task<Result<String, String>>`). Each tool generates its
//! `input_schema` from a typed struct via `schemars` and parses input with
//! `serde_json::from_value`. The registry stores `Arc<dyn AgentTool>`.

pub mod permission;

use std::collections::BTreeMap;
use std::sync::Arc;

use gpui::{App, Task};
use tokio_util::sync::CancellationToken;

use crate::language_model::LanguageModelRequestTool;

pub use permission::{PermissionCache, PermissionDecision, ToolAuthorizationResponse};

/// Cloneable handle for live tool output. Tools that stream (e.g. `bash`)
/// call [`ToolOutputSink::try_emit`] per output chunk; the owning `Thread`
/// drains the receiver into `ThreadEvent::ToolOutput` for the UI. The channel
/// is bounded; overflow drops chunks silently — real-time liveness is
/// preferred over completeness, and the final `ThreadEvent::ToolResult`
/// carries the canonical (truncated) full output.
#[derive(Clone)]
pub struct ToolOutputSink {
    tool_call_id: Arc<str>,
    tx: async_channel::Sender<String>,
}

impl ToolOutputSink {
    /// Create a sink bound to a tool-call id and its matching receiver.
    pub fn channel(tool_call_id: Arc<str>) -> (Self, async_channel::Receiver<String>) {
        let (tx, rx) = async_channel::bounded(256);
        (Self { tool_call_id, tx }, rx)
    }

    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }

    /// Best-effort emit; dropped silently if the channel is full or closed.
    pub fn try_emit(&self, chunk: &str) {
        let _ = self.tx.try_send(chunk.to_string());
    }
}

/// Tool trait. `run` executes on the gpui executor and returns a `Task`.
pub trait AgentTool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema sent to the model.
    fn input_schema(&self) -> serde_json::Value;
    /// Whether the tool requires user approval before running. Takes the
    /// parsed-call `input` so tools like `bash` can gate approval on a knob
    /// (e.g. `unsandboxed: true`): the sandboxed default is safe to run
    /// without approval, only the unsandboxed escalation needs a human.
    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        false
    }
    /// Run the tool. `cancel` is the current turn's cancellation token; long-running
    /// tools (e.g. `bash`) select on it so a user-initiated stop reaps the work
    /// promptly. `Ok(output)` is a normal output string; `Err(output)` is an error
    /// output string (still fed back to the model).
    fn run(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>>;

    /// Streaming variant. Streaming tools (e.g. `bash`) override this to emit
    /// live output chunks via `sink` while running; the returned `Task` still
    /// yields the canonical final result. The default delegates to [`run`],
    /// ignoring the sink, so non-streaming tools are unaffected.
    fn run_streaming(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        sink: ToolOutputSink,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let _ = sink;
        self.run(input, cancel, cx)
    }
}

pub type AnyAgentTool = Arc<dyn AgentTool>;

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, AnyAgentTool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: AnyAgentTool) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&AnyAgentTool> {
        self.tools.get(name)
    }

    /// Build the `LanguageModelRequestTool` list sent to the model.
    pub fn to_request_tools(&self) -> Vec<LanguageModelRequestTool> {
        self.tools
            .values()
            .map(|tool| LanguageModelRequestTool {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
                use_input_streaming: false,
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
