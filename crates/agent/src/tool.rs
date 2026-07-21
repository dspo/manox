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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, Entity, Task};
use tokio_util::sync::CancellationToken;

use crate::language_model::{AnyLanguageModel, LanguageModelRequestTool};

pub use permission::{PermissionCache, PermissionDecision, ToolAuthorizationResponse};

/// Read-only runtime identity a tool reads from its owning `Thread`, passed
/// into [`AgentTool::run`] / [`AgentTool::run_streaming`] per invocation.
///
/// Tools no longer hold a `WeakEntity<Thread>` reverse-reference: the snapshot
/// is built once by `Thread::run_tool_inner` before the call, so the dependency
/// direction is `Thread → tools` (the thread knows about its tools; tools do
/// not know about the thread). The borrow lives only for the synchronous `run`
/// call — tools extract owned data before spawning their `Task`, so the
/// `&dyn ToolContext` never crosses an await boundary.
pub trait ToolContext {
    fn thread_id(&self) -> &str;
    fn cwd(&self) -> &Path;
    fn project(&self) -> Option<&PathBuf>;
    fn model(&self) -> Option<&AnyLanguageModel>;
    fn max_turns(&self) -> Option<u32>;
    fn turn_count(&self) -> u32;
    fn depth(&self) -> u32;
    /// Human-readable label of the owning agent: "lead" for the main thread,
    /// the subagent_type for an `agent`-spawned sub-agent, the member name for
    /// a team member. Used as the write-lock owner so conflict errors name the
    /// agent holding the file.
    fn agent_label(&self) -> &str;
    /// The active team this thread leads or belongs to, if any. `None` for
    /// non-team threads. Team tools reach the shared task list and the message
    /// router through this.
    fn team(&self) -> Option<&Entity<crate::team::Team>>;
    /// Whether the owning thread is in YOLO / AutoReview mode (bash uses this
    /// to force the unsandboxed branch without a per-call escalation flag).
    fn yolo(&self) -> bool;
    /// The loopback port of the thread's in-process network proxy, when the
    /// sandbox network policy is `Restricted`. The sandboxed bash command
    /// is wrapped so the seatbelt only allows outbound to this port, and
    /// `HTTP_PROXY`/`HTTPS_PROXY` env vars point here. `None` when the
    /// policy is `Blocked` or `Unrestricted` (no proxy running).
    fn network_proxy_port(&self) -> Option<u16>;
}

/// Owned snapshot of a `Thread`'s read-only fields, built per tool call. The
/// `AnyLanguageModel` is an `Arc` clone, so the snapshot is cheap; tools read
/// what they need off `&self` synchronously and move owned data into their
/// `Task`.
pub struct ToolContextSnapshot {
    thread_id: String,
    cwd: PathBuf,
    project: Option<PathBuf>,
    model: Option<AnyLanguageModel>,
    max_turns: Option<u32>,
    turn_count: u32,
    depth: u32,
    agent_label: String,
    team: Option<Entity<crate::team::Team>>,
    yolo: bool,
    network_proxy_port: Option<u16>,
}

impl ToolContextSnapshot {
    /// Build a snapshot from a `Thread`'s current state. Called by
    /// `run_tool_inner` immediately before invoking the tool.
    pub fn from_thread(t: &crate::thread::Thread) -> Self {
        Self {
            thread_id: t.id.0.clone(),
            cwd: t.cwd().to_path_buf(),
            project: t.project().cloned(),
            model: t.model().cloned(),
            max_turns: t.max_turns(),
            turn_count: t.turn_count(),
            depth: t.depth(),
            agent_label: t.agent_label().to_string(),
            team: t.team().cloned(),
            yolo: t.approval_mode() == crate::thread::ApprovalMode::Yolo,
            network_proxy_port: t.network_proxy_port(),
        }
    }
}

impl ToolContext for ToolContextSnapshot {
    fn thread_id(&self) -> &str {
        &self.thread_id
    }
    fn cwd(&self) -> &Path {
        &self.cwd
    }
    fn project(&self) -> Option<&PathBuf> {
        self.project.as_ref()
    }
    fn model(&self) -> Option<&AnyLanguageModel> {
        self.model.as_ref()
    }
    fn max_turns(&self) -> Option<u32> {
        self.max_turns
    }
    fn turn_count(&self) -> u32 {
        self.turn_count
    }
    fn depth(&self) -> u32 {
        self.depth
    }
    fn agent_label(&self) -> &str {
        &self.agent_label
    }
    fn team(&self) -> Option<&Entity<crate::team::Team>> {
        self.team.as_ref()
    }
    fn yolo(&self) -> bool {
        self.yolo
    }
    fn network_proxy_port(&self) -> Option<u16> {
        self.network_proxy_port
    }
}

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
        ctx: &dyn ToolContext,
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
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let _ = sink;
        self.run(input, cancel, ctx, cx)
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

    /// Plan-mode tool set: only tools whose `is_read_only()` returns true.
    /// Write/exec tools (`write_file`, `edit_file`, `bash`) are excluded; the
    /// `agent` tool is read-only (`SpawnAgentTool::is_read_only`), so it
    /// survives — letting the main thread delegate research to the `Explore`
    /// sub-agent with isolated context.
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
    use crate::tools::base_tools;
    use std::path::PathBuf;

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::default();
        for t in base_tools(Arc::new(PathBuf::from("."))) {
            r.register(t);
        }
        r
    }

    /// Stub tool with a configurable name so tests can probe ordering without
    /// depending on the built-in tool set.
    struct StubTool {
        name: &'static str,
    }

    impl AgentTool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn run(
            &self,
            _input: serde_json::Value,
            _cancel: CancellationToken,
            _ctx: &dyn ToolContext,
            _cx: &mut App,
        ) -> Task<Result<String, String>> {
            Task::ready(Ok("stub".to_string()))
        }
    }

    fn stub(name: &'static str) -> AnyAgentTool {
        Arc::new(StubTool { name }) as AnyAgentTool
    }

    /// The request-tool list must be lexicographically sorted by name. The
    /// `BTreeMap` backing `ToolRegistry` gives this for free; this test pins
    /// the invariant so a future switch to `HashMap` (which would silently
    /// bust the provider's prefix cache by reordering tool specs turn-over-turn)
    /// is caught.
    #[test]
    fn to_request_tools_is_lexicographically_sorted() {
        let r = registry();
        let names: Vec<String> = r
            .to_request_tools()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "tool list must be sorted by name: {names:?}");
    }

    /// Registering a name that sorts before every built-in keeps the list
    /// sorted — the cache-stable ordering is preserved regardless of
    /// registration order.
    #[test]
    fn registering_out_of_order_name_stays_sorted() {
        let mut r = registry();
        // "000_stub" sorts before all built-in tool names; registered last but
        // must appear first in the output.
        r.register(stub("000_stub"));
        r.register(stub("zzz_stub"));
        let names: Vec<String> = r
            .to_request_tools()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "sorted after out-of-order register: {names:?}"
        );
        assert_eq!(names.first(), Some(&"000_stub".to_string()));
        assert_eq!(names.last(), Some(&"zzz_stub".to_string()));
    }

    #[test]
    fn filtered_excludes_write_tools_in_plan_mode() {
        let r = registry();
        let full = r.to_request_tools();
        let ro = r.to_request_tools_read_only();

        let full_names: Vec<&str> = full.iter().map(|t| t.name.as_str()).collect();
        let ro_names: Vec<&str> = ro.iter().map(|t| t.name.as_str()).collect();

        // Write/exec tools present in full are absent from the filtered set.
        for blocked in ["Write", "Edit", "Bash"] {
            assert!(full_names.contains(&blocked), "{blocked} in full set");
            assert!(
                !ro_names.contains(&blocked),
                "{blocked} leaked into plan-mode set"
            );
        }
        // Read-only tools survive the filter.
        for allowed in ["Read", "List", "Grep", "Glob", "AskUserQuestion"] {
            assert!(
                ro_names.contains(&allowed),
                "{allowed} missing from plan-mode set"
            );
        }
        // The read-only set is strictly smaller than the full set.
        assert!(ro.len() < full.len());
    }
}
