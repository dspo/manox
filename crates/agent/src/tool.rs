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
pub mod plan_mode;

use std::collections::BTreeMap;
use std::sync::Arc;

use gpui::{App, Task};
use tokio_util::sync::CancellationToken;

use crate::language_model::LanguageModelRequestTool;

pub use permission::{PermissionCache, PermissionDecision, ToolAuthorizationResponse};
pub use plan_mode::{ExitPlanModeTool, PlanApprovalResponse, exit_plan_mode_request_tool};

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
    /// Whether this tool only reads and never mutates the world. Plan mode
    /// uses this to hide write tools from the model entirely (a filtered
    /// request-tool list), so the model cannot even attempt them. Default
    /// `false`; read-only tools override to `true`.
    fn is_read_only(&self) -> bool {
        false
    }
    /// Whether the authorization flow is the tool's execution path rather than
    /// a permission gate — the tool produces its result from the
    /// `ToolAuthorizationResponse` itself and its `run` body is unreachable.
    /// YOLO mode and `AlwaysAllow` must not bypass such tools, or the model
    /// would hit the unreachable `run` and lose the user's input. Only
    /// `AskUserQuestion` overrides this.
    fn requires_user_input(&self) -> bool {
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

    /// Filtered tool list: only tools whose `name` appears in `allowed`. Used by
    /// slash commands whose `allowed-tools` frontmatter narrows the turn's tool
    /// set. Empty `allowed` yields an empty list — callers should fall back to
    /// [`to_request_tools`] when the filter is unset rather than call this with
    /// an empty slice.
    pub fn to_request_tools_filtered(&self, allowed: &[String]) -> Vec<LanguageModelRequestTool> {
        self.tools
            .values()
            .filter(|tool| allowed.iter().any(|a| a == tool.name()))
            .map(|tool| LanguageModelRequestTool {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
                use_input_streaming: false,
            })
            .collect()
    }

    /// Plan-mode filter: only tools whose `is_read_only()` returns true. Write
    /// tools (`write_file`, `edit_file`, `bash`, `agent`) are excluded, leaving
    /// the read-only allowlist. The `exit_plan_mode` tool is appended separately
    /// by the caller — it is not in the registry.
    pub fn to_request_tools_read_only(&self) -> Vec<LanguageModelRequestTool> {
        self.tools
            .values()
            .filter(|t| t.is_read_only())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::Thread;
    use crate::tools::base_tools;
    use gpui::WeakEntity;
    use std::path::PathBuf;

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::default();
        for t in base_tools(
            Arc::new(PathBuf::from(".")),
            WeakEntity::<Thread>::new_invalid(),
        ) {
            r.register(t);
        }
        r
    }

    #[test]
    fn filtered_excludes_write_tools_in_plan_mode() {
        let r = registry();
        let full = r.to_request_tools();
        let ro = r.to_request_tools_read_only();

        let full_names: Vec<&str> = full.iter().map(|t| t.name.as_str()).collect();
        let ro_names: Vec<&str> = ro.iter().map(|t| t.name.as_str()).collect();

        // Write/exec tools present in full are absent from the filtered set.
        for blocked in ["write_file", "edit_file", "bash"] {
            assert!(full_names.contains(&blocked), "{blocked} in full set");
            assert!(
                !ro_names.contains(&blocked),
                "{blocked} leaked into plan-mode set"
            );
        }
        // Read-only tools survive the filter.
        for allowed in [
            "read_file",
            "list_directory",
            "grep",
            "glob",
            "AskUserQuestion",
        ] {
            assert!(
                ro_names.contains(&allowed),
                "{allowed} missing from plan-mode set"
            );
        }
        // exit_plan_mode is NOT in the registry — it is appended by the caller.
        assert!(!ro_names.contains(&"exit_plan_mode"));
        assert!(ro.len() < full.len());
    }
}
