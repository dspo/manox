//! Turn loop —— Cx Agent 一次"用户输入 → 流式输出"的核心。

use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;

use crate::Selection;
use crate::cx_agent::approval::{ApprovalDecision, ApprovalMode, decide};
use crate::cx_agent::config::CxAgentRuntimeConfig;
use crate::cx_agent::events::CxStreamEvent;
use crate::cx_agent::history::{CxContent, CxMessage};
use crate::cx_agent::provider::mapping::build as build_adapter;
use crate::cx_agent::provider::{CxTurnRequest, ProviderAdapter};
use crate::cx_agent::rollout::{Rollout, TokenUsageRecord};
use crate::cx_agent::tools::Registry;
use crate::cx_agent::tui::approval_prompt::ApprovalRequest;
use crate::cx_agent::tui::chat::{ChatApp, ChatEvent, RunOutcome};

const DEFAULT_SYSTEM: &str = "你是 Cx Agent，一个在终端内运行的编码助手。\n\
- 用简洁中文回应；代码块原样保留。\n\
- 可按需使用内置工具：read_file / write_file / edit_file / bash / grep / glob。\n\
- 如果 write/execute 类工具需要审批而被拒绝，你会收到 tool result 错误，需基于该结果继续。";
const MAX_TOOL_ROUNDS: usize = 8;
const MAX_TOOL_RESULT_CHARS: usize = 24_000;

pub async fn run(selection: Selection, _passthrough_args: Vec<String>) -> Result<()> {
    let runtime_config = CxAgentRuntimeConfig::from_selection(&selection)?;
    let adapter = build_adapter(runtime_config.adapter.clone())?;
    let mut rollout = Rollout::open(&runtime_config.adapter)?;
    let registry = Registry::with_builtins()?;

    let mut chat = ChatApp::new(
        runtime_config.adapter.provider_name.clone(),
        runtime_config.adapter.model_id.clone(),
        runtime_config.adapter.wire_api.display().to_string(),
        runtime_config.approval_mode,
        rollout.session_id.clone(),
        rollout.path.clone(),
    );

    let mut history: Vec<CxMessage> = Vec::new();

    while let ChatEvent::Input(text) = chat.read_user_input().await? {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        rollout.write_user_message(trimmed)?;
        history.push(CxMessage::user_text(trimmed));
        let outcome = run_one_turn(
            &mut chat,
            adapter.as_ref(),
            &registry,
            &runtime_config.adapter.model_id,
            runtime_config.approval_mode,
            &mut history,
            &mut rollout,
        )
        .await;
        if let Err(e) = outcome {
            chat.show_error(&format!("{e:?}"));
            rollout.write_error(&format!("{e:?}")).ok();
        }
    }

    chat.shutdown()?;
    Ok(())
}

async fn run_one_turn(
    chat: &mut ChatApp,
    adapter: &dyn ProviderAdapter,
    registry: &Registry,
    model_id: &str,
    approval_mode: ApprovalMode,
    history: &mut Vec<CxMessage>,
    rollout: &mut Rollout,
) -> Result<()> {
    for tool_round in 0..MAX_TOOL_ROUNDS {
        let request = CxTurnRequest {
            system: Some(DEFAULT_SYSTEM.to_string()),
            history: history.clone(),
            tools: registry.definitions(),
            model_id: model_id.to_string(),
            max_tokens: Some(2048),
        };

        let started = Instant::now();
        let mut stream = adapter
            .stream_turn(request)
            .await
            .context("调用 ProviderAdapter::stream_turn 失败")?;

        chat.begin_assistant();
        let assistant_turn = collect_assistant_turn(chat, &mut stream).await?;
        chat.end_assistant(started.elapsed(), assistant_turn.usage);

        if !assistant_turn.content.is_empty() {
            history.push(CxMessage::Assistant {
                content: assistant_turn.content.clone(),
            });
        }
        if !assistant_turn.text_output.is_empty() {
            rollout.write_assistant_message(&assistant_turn.text_output)?;
        }
        rollout.write_token_usage(assistant_turn.usage)?;

        if assistant_turn.tool_calls.is_empty() {
            return Ok(());
        }

        execute_tool_calls(
            chat,
            registry,
            approval_mode,
            &assistant_turn.tool_calls,
            history,
        )
        .await?;

        if tool_round + 1 == MAX_TOOL_ROUNDS {
            bail!("tool 调用轮数超过上限 {MAX_TOOL_ROUNDS}");
        }
    }
    unreachable!("tool loop should return before exhausting the loop")
}

#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
struct AssistantTurn {
    content: Vec<CxContent>,
    text_output: String,
    usage: TokenUsageRecord,
    tool_calls: Vec<PendingToolCall>,
}

async fn collect_assistant_turn(
    chat: &mut ChatApp,
    stream: &mut futures::stream::BoxStream<'_, Result<CxStreamEvent>>,
) -> Result<AssistantTurn> {
    let mut assistant_turn = AssistantTurn::default();
    let mut reasoning_buf = String::new();

    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                let msg = format!("{err}");
                let _ = chat
                    .handle_stream_event(&Ok(CxStreamEvent::Error(msg.clone())))
                    .await?;
                return Err(anyhow!("流式响应失败: {msg}"));
            }
        };
        let outcome = chat.handle_stream_event(&Ok(event.clone())).await?;
        match event {
            CxStreamEvent::TextDelta(text) => assistant_turn.text_output.push_str(&text),
            CxStreamEvent::ReasoningDelta(text) => reasoning_buf.push_str(&text),
            CxStreamEvent::Usage {
                input,
                output,
                cache_read,
                cache_write,
                reasoning,
            } => {
                assistant_turn.usage = TokenUsageRecord {
                    input,
                    output,
                    cache_read,
                    cache_write,
                    reasoning,
                };
            }
            CxStreamEvent::ToolCallDone {
                id,
                name,
                arguments,
            } => {
                assistant_turn.content.push(CxContent::ToolCall {
                    id: id.clone(),
                    call_id: Some(id.clone()),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
                assistant_turn.tool_calls.push(PendingToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            CxStreamEvent::ToolCallStart { .. }
            | CxStreamEvent::ToolCallArgsDelta { .. }
            | CxStreamEvent::Done => {}
            CxStreamEvent::Error(msg) => return Err(anyhow!("流式响应失败: {msg}")),
        }
        if matches!(outcome, RunOutcome::Aborted) {
            return Err(anyhow!("assistant 响应被中止"));
        }
    }

    if !reasoning_buf.is_empty() {
        assistant_turn.content.insert(
            0,
            CxContent::Reasoning {
                text: reasoning_buf,
            },
        );
    }
    if !assistant_turn.text_output.is_empty() {
        assistant_turn.content.push(CxContent::Text {
            text: assistant_turn.text_output.clone(),
        });
    }
    Ok(assistant_turn)
}

async fn execute_tool_calls(
    chat: &mut ChatApp,
    registry: &Registry,
    approval_mode: ApprovalMode,
    tool_calls: &[PendingToolCall],
    history: &mut Vec<CxMessage>,
) -> Result<()> {
    for tool_call in tool_calls {
        let tool_result =
            execute_single_tool_call(chat, registry, approval_mode, tool_call).await?;
        history.push(CxMessage::ToolResult {
            call_id: tool_call.id.clone(),
            name: Some(tool_call.name.clone()),
            content: tool_result.content,
            is_error: tool_result.is_error,
        });
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ToolExecutionResult {
    content: String,
    is_error: bool,
}

async fn execute_single_tool_call(
    chat: &mut ChatApp,
    registry: &Registry,
    approval_mode: ApprovalMode,
    tool_call: &PendingToolCall,
) -> Result<ToolExecutionResult> {
    let Some(tool) = registry.get(&tool_call.name) else {
        return Ok(ToolExecutionResult {
            content: format!(
                "Tool `{}` is not available in this session.",
                tool_call.name
            ),
            is_error: true,
        });
    };

    let decision = match decide(approval_mode, tool.category()) {
        ApprovalDecision::Allow => ApprovalDecision::Allow,
        ApprovalDecision::Ask => chat.prompt_approval(ApprovalRequest::from_json(
            tool_call.name.clone(),
            tool.category(),
            &tool_call.arguments,
        ))?,
        ApprovalDecision::Deny { reason } => ApprovalDecision::Deny { reason },
    };

    match decision {
        ApprovalDecision::Allow => {
            let outcome = match registry
                .invoke(&tool_call.name, tool_call.arguments.clone())
                .await
            {
                Ok(output) => ToolExecutionResult {
                    content: clamp_tool_result(output),
                    is_error: false,
                },
                Err(err) => ToolExecutionResult {
                    content: clamp_tool_result(format!("Tool `{}` failed: {err}", tool_call.name)),
                    is_error: true,
                },
            };
            if outcome.is_error {
                chat.show_error(&outcome.content);
            }
            Ok(outcome)
        }
        ApprovalDecision::Ask => {
            unreachable!("approval decisions should be resolved before execution")
        }
        ApprovalDecision::Deny { reason } => Ok(ToolExecutionResult {
            content: clamp_tool_result(format!(
                "Tool `{}` was denied by the user: {}",
                tool_call.name, reason
            )),
            is_error: true,
        }),
    }
}

fn clamp_tool_result(content: String) -> String {
    let char_count = content.chars().count();
    if char_count <= MAX_TOOL_RESULT_CHARS {
        return content;
    }
    let truncated: String = content.chars().take(MAX_TOOL_RESULT_CHARS).collect();
    format!("{truncated}\n\n[truncated to {MAX_TOOL_RESULT_CHARS} chars from {char_count}]")
}

#[cfg(test)]
mod tests {
    use super::{MAX_TOOL_RESULT_CHARS, clamp_tool_result};

    #[test]
    fn clamp_tool_result_preserves_short_content() {
        assert_eq!(clamp_tool_result("hello".to_string()), "hello");
    }

    #[test]
    fn clamp_tool_result_truncates_long_content() {
        let input = "a".repeat(MAX_TOOL_RESULT_CHARS + 10);
        let output = clamp_tool_result(input);
        assert!(output.contains("truncated to"));
        assert!(output.len() > MAX_TOOL_RESULT_CHARS);
    }
}
