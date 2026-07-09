//! The `enter_plan_mode` / `exit_plan_mode` tools and the plan-approval
//! response type.
//!
//! Plan mode is entered either by the user (`/plan`, the `+` menu) or by the
//! model calling `enter_plan_mode`. `Thread::run_tool_inner` intercepts that
//! call before the registry lookup (`run_enter_plan_mode`): it flips
//! `plan_mode` on and appends a tool result steering the model toward
//! read-only research. No approval — it is a mode transition the model
//! initiates, not a write.
//!
//! In plan mode the model researches with read-only tools, then calls
//! `exit_plan_mode { plan }` to submit its plan. `Thread::run_tool_inner`
//! intercepts that call before the registry lookup and runs the approval
//! handshake (`run_plan_approval`): it emits `ThreadEvent::PlanProposed`,
//! parks on a oneshot until the user approves/rejects, and either exits plan
//! mode (approve) or stays in plan mode for revision (reject).
//!
//! Neither tool is registered in the `ToolRegistry` — their `run` bodies are
//! stubs, reached only if the intercept is bypassed (a safety net).
//! `Thread::build_completion_request` synthesizes each request-tool definition
//! via [`enter_plan_mode_request_tool`] / [`exit_plan_mode_request_tool`] for
//! the state that advertises it: `enter_plan_mode` in the non-plan, main-thread
//! request; `exit_plan_mode` in the plan-mode request. So the model sees each
//! tool only in the state where it is meaningful.

use gpui::{App, Task};
use tokio_util::sync::CancellationToken;

use crate::language_model::LanguageModelRequestTool;
use crate::tool::AgentTool;

/// The model's only way to surface a plan and leave plan mode.
pub struct ExitPlanModeTool;

impl AgentTool for ExitPlanModeTool {
    fn name(&self) -> &str {
        "exit_plan_mode"
    }

    fn description(&self) -> &str {
        "Submit your proposed plan and ask the user for approval. Only available in plan \
         mode. Calling it pauses the conversation while the user approves or rejects: \
         approval exits plan mode and begins execution; rejection keeps you in plan mode \
         for revision."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The full plan text: step-by-step implementation, the tools each step will use, and potential risks."
                }
            },
            "required": ["plan"]
        })
    }

    // The approval handshake is driven by `Thread::run_tool_inner`; this `run`
    // is a stub reached only if the intercept is bypassed.
    fn run(
        &self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        _cx: &mut App,
    ) -> Task<Result<String, String>> {
        gpui::Task::ready(Err(
            "exit_plan_mode must be intercepted by the thread in plan mode".to_string(),
        ))
    }
}

/// Build the `LanguageModelRequestTool` advertised to the model in plan mode.
/// The tool is not in the registry, so this is the only place its definition
/// is minted.
pub fn exit_plan_mode_request_tool() -> LanguageModelRequestTool {
    let tool = ExitPlanModeTool;
    LanguageModelRequestTool {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        input_schema: tool.input_schema(),
        use_input_streaming: false,
    }
}

/// The model's way to proactively enter plan mode mid-turn when it judges the
/// task non-trivial. Only advertised on the main thread (depth 0) and only
/// while not already in plan mode.
pub struct EnterPlanModeTool;

impl AgentTool for EnterPlanModeTool {
    fn name(&self) -> &str {
        "enter_plan_mode"
    }

    fn description(&self) -> &str {
        "Proactively enter plan mode when the task is non-trivial: multi-file or \
         cross-module changes, multiple viable approaches, architectural decisions, \
         unclear requirements, or refactoring an existing system. Transitions you to \
         read-only tools so you can research the codebase and produce a plan, then call \
         exit_plan_mode to submit it for approval. Call this alone or alongside read-only \
         tools — never alongside write tools. Not available to sub-agents."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    // The mode transition is driven by `Thread::run_tool_inner`; this `run` is
    // a stub reached only if the intercept is bypassed.
    fn run(
        &self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        _cx: &mut App,
    ) -> Task<Result<String, String>> {
        gpui::Task::ready(Err(
            "enter_plan_mode must be intercepted by the thread".to_string()
        ))
    }
}

/// Build the `LanguageModelRequestTool` advertised to the main thread while it
/// is not in plan mode. The tool is not in the registry, so this is the only
/// place its definition is minted.
pub fn enter_plan_mode_request_tool() -> LanguageModelRequestTool {
    let tool = EnterPlanModeTool;
    LanguageModelRequestTool {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        input_schema: tool.input_schema(),
        use_input_streaming: false,
    }
}

/// The user's verdict on a submitted plan. Resolves the oneshot parked in
/// `Thread::pending_plan_approval`, mirroring `ToolAuthorizationResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanApprovalResponse {
    /// Exit plan mode and begin executing the approved plan.
    Approve,
    /// Stay in plan mode and let the model revise.
    Reject,
}
