//! The `exit_plan_mode` tool and the plan-approval response type.
//!
//! In plan mode the model researches with read-only tools, then calls
//! `exit_plan_mode { plan }` to submit its plan. `Thread::run_tool_inner`
//! intercepts that call before the registry lookup and runs the approval
//! handshake (`run_plan_approval`): it emits `ThreadEvent::PlanProposed`,
//! parks on a oneshot until the user approves/rejects, and either exits plan
//! mode (approve) or stays in plan mode for revision (reject).
//!
//! `ExitPlanModeTool` itself is never registered in the `ToolRegistry` — its
//! `run` is a stub, reached only if the intercept is bypassed (a safety net).
//! `Thread::build_completion_request` synthesizes the request-tool definition
//! via [`exit_plan_mode_request_tool`] when in plan mode, so the model sees the
//! tool only while planning.

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
        "提交你制定好的计划并请求用户批准。仅在计划模式下可用。调用后对话会暂停，\
         等待用户批准或拒绝：批准则退出计划模式并开始执行；拒绝则继续在计划模式下修订。"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "完整的计划文本：分步实施方案、每步将使用的工具、潜在风险。"
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

/// The user's verdict on a submitted plan. Resolves the oneshot parked in
/// `Thread::pending_plan_approval`, mirroring `ToolAuthorizationResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanApprovalResponse {
    /// Exit plan mode and begin executing the approved plan.
    Approve,
    /// Stay in plan mode and let the model revise.
    Reject,
}
