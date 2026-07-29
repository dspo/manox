// Agent loop — the pure dual-loop state machine.
//
// This is the heart of the harness. It takes a stream function, a context,
// and a config, and drives the conversation through turns of LLM streaming
// and tool execution until the agent naturally stops or is aborted.
//
// Dual-loop structure:
//   Outer loop — continues when queued follow-up messages arrive after the
//                agent would otherwise stop.
//   Inner loop — processes tool calls and steering messages within a single
//                turn.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentToolResult, execute_tool_calls};
use crate::types::{AgentMessage, AgentEvent, AgentContext, AgentLoopConfig, ContentBlock, StopReason};

/// Sink for agent lifecycle events emitted during the loop.
pub trait EventSink: Send {
    fn emit(&self, event: AgentEvent);
}

/// The function that streams an assistant response from an LLM.
///
/// The harness doesn't know or care which provider is on the other end. The
/// stream function sends events through an mpsc channel. The loop collects
/// them on the receiver side, building the assistant message and forwarding
/// events to subscribers.
#[async_trait::async_trait]
pub trait StreamFn: Send + Sync {
    /// Stream the assistant's response for the given context.
    ///
    /// Sends lifecycle events through `event_tx` as the response streams in.
    /// Returns the final assistant message when the stream completes.
    async fn stream(
        &self,
        context: &AgentContext,
        signal: CancellationToken,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error>;
}

/// Run the agent loop with new prompt messages.
///
/// The prompts are prepended to the context, and the loop runs until the
/// agent naturally stops, is aborted, or hits the max turn limit.
pub async fn run_loop(
    prompts: &[AgentMessage],
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: Arc<dyn StreamFn>,
    sink: &(dyn EventSink + Send + Sync),
) -> Result<Vec<AgentMessage>, anyhow::Error> {
    let signal = signal.unwrap_or_else(CancellationToken::new);
    let mut new_messages: Vec<AgentMessage> = prompts.to_vec();

    // Prepend prompts to the context.
    let mut current_messages: Vec<AgentMessage> = context.messages.clone();
    current_messages.extend_from_slice(prompts);
    context.messages = current_messages;

    run_loop_inner(context, &mut new_messages, config, &signal, stream_fn, sink).await?;
    Ok(new_messages)
}

/// Continue the agent loop from the current context without adding new messages.
pub async fn run_loop_continue(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: Arc<dyn StreamFn>,
    sink: &(dyn EventSink + Send + Sync),
) -> Result<Vec<AgentMessage>, anyhow::Error> {
    let signal = signal.unwrap_or_else(CancellationToken::new);

    if context.messages.is_empty() {
        anyhow::bail!("Cannot continue: no messages in context");
    }
    if matches!(context.messages.last(), Some(AgentMessage::Assistant { .. })) {
        anyhow::bail!("Cannot continue from message role: assistant");
    }

    let mut new_messages: Vec<AgentMessage> = Vec::new();
    run_loop_inner(context, &mut new_messages, config, &signal, stream_fn, sink).await?;
    Ok(new_messages)
}

/// The main loop logic shared by `run_loop` and `run_loop_continue`.
async fn run_loop_inner(
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig,
    signal: &CancellationToken,
    stream_fn: Arc<dyn StreamFn>,
    sink: &(dyn EventSink + Send + Sync),
) -> Result<(), anyhow::Error> {
    sink.emit(AgentEvent::AgentStart);
    let mut turn_count: usize = 0;

    // Check for steering messages at start (may have been queued while idle).
    let mut pending_messages: Vec<AgentMessage> = config
        .get_steering_messages
        .as_ref()
        .map(|f| f())
        .unwrap_or_default();

    // Outer loop: continues when queued follow-up messages arrive after the
    // agent would otherwise stop.
    loop {
        if signal.is_cancelled() {
            break;
        }

        let mut has_more_tool_calls = true;

        // Inner loop: process tool calls and steering messages.
        while has_more_tool_calls || !pending_messages.is_empty() {
            if signal.is_cancelled() {
                break;
            }

            sink.emit(AgentEvent::TurnStart);
            turn_count += 1;

            // Inject pending steering messages before the next assistant response.
            if !pending_messages.is_empty() {
                for msg in pending_messages.drain(..) {
                    sink.emit(AgentEvent::MessageStart {
                        message: Box::new(msg.clone()),
                    });
                    sink.emit(AgentEvent::MessageEnd {
                        message: Box::new(msg.clone()),
                    });
                    context.messages.push(msg.clone());
                    new_messages.push(msg);
                }
            }

            // Stream assistant response.
            let message = stream_assistant_response(context, signal, Arc::clone(&stream_fn), sink).await?;

            new_messages.push(message.clone());
            context.messages.push(message.clone());

            let stop_reason = match &message {
                AgentMessage::Assistant { stop_reason, .. } => *stop_reason,
                _ => None,
            };

            if stop_reason == Some(StopReason::Error) || stop_reason == Some(StopReason::Aborted) {
                sink.emit(AgentEvent::TurnEnd {
                    message: Box::new(message),
                    tool_results: Vec::new(),
                });
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                return Ok(());
            }

            // Extract tool calls and execute them.
            let tool_calls: Vec<(&str, &str, serde_json::Value)> = match &message {
                AgentMessage::Assistant { content, .. } => content
                    .iter()
                    .filter_map(|block| {
                        if let ContentBlock::ToolCall { id, name, arguments } = block {
                            Some((id.as_str(), name.as_str(), arguments.clone()))
                        } else {
                            None
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            };

            let mut tool_results: Vec<AgentMessage> = Vec::new();
            has_more_tool_calls = false;

            if !tool_calls.is_empty() {
                // If the response was truncated, fail all tool calls.
                let (executed, result_messages) = if stop_reason == Some(StopReason::Length) {
                    fail_tool_calls_from_truncated(&tool_calls, sink)
                } else {
                    execute_tool_calls(
                        &tool_calls,
                        &context.tools,
                        signal.clone(),
                        &NoopToolContext,
                        config.sequential_tool_execution,
                    )
                    .await
                };

                // Check if all finalized calls want to terminate.
                let all_terminate = executed.iter().all(|ec| ec.result.terminate);

                tool_results = result_messages;
                has_more_tool_calls = !all_terminate;

                for result in &tool_results {
                    context.messages.push(result.clone());
                    new_messages.push(result.clone());
                }
            }

            sink.emit(AgentEvent::TurnEnd {
                message: Box::new(message),
                tool_results: tool_results.clone(),
            });

            // Apply next-turn context update.
            if let Some(ref prepare_next_turn) = config.prepare_next_turn {
                if let Some(updated) = prepare_next_turn(context) {
                    *context = updated;
                }
            }

            // Check early stop.
            if let Some(ref should_stop) = config.should_stop_after_turn {
                let last_msg = context.messages.last().cloned();
                if let Some(last_msg) = last_msg {
                    if should_stop(&last_msg, &tool_results) {
                        sink.emit(AgentEvent::AgentEnd {
                            messages: new_messages.clone(),
                        });
                        return Ok(());
                    }
                }
            }

            // Check max turns.
            if let Some(max_turns) = config.max_turns {
                if turn_count >= max_turns {
                    sink.emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    });
                    return Ok(());
                }
            }

            // Poll steering queue.
            pending_messages = config
                .get_steering_messages
                .as_ref()
                .map(|f| f())
                .unwrap_or_default();
        }

        // Inner loop exhausted — check for follow-up messages.
        let follow_up = config
            .get_follow_up_messages
            .as_ref()
            .map(|f| f())
            .unwrap_or_default();

        if !follow_up.is_empty() {
            pending_messages = follow_up;
            continue;
        }

        break;
    }

    sink.emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    });
    Ok(())
}

/// Stream an assistant response from the LLM and emit lifecycle events.
///
/// Creates an mpsc channel, spawns the stream function on a tokio task,
/// and collects events on the receiver side. This avoids the Cell/RefCell
/// issues that arise from Fn closures — state accumulation happens on the
/// receiver side without any Sync requirements.
async fn stream_assistant_response(
    context: &AgentContext,
    signal: &CancellationToken,
    stream_fn: Arc<dyn StreamFn>,
    sink: &(dyn EventSink + Send + Sync),
) -> Result<AgentMessage, anyhow::Error> {
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);

    // Clone what the spawned task needs.
    let ctx = context.clone();
    let sig = signal.clone();

    // Spawn the stream function so it can send events concurrently.
    let stream_handle = tokio::spawn(async move {
        stream_fn.stream(&ctx, sig, event_tx).await
    });

    // Accumulate streaming state on the receiver side.
    let mut first = true;

    // Process events as they arrive from the channel.
    // The channel closes when the sender is dropped (stream completes or errors).
    while let Some(event) = event_rx.recv().await {
        match &event {
            AgentEvent::MessageStart { .. } => {
                first = false;
            }
            AgentEvent::MessageUpdate { .. } => {}
            _ => {}
        }
        sink.emit(event);
    }

    // Wait for the stream to complete and get the final message.
    let message = stream_handle.await??;

    // Emit message_start/message_end if not already emitted by the stream.
    if first {
        sink.emit(AgentEvent::MessageStart {
            message: Box::new(message.clone()),
        });
    }
    sink.emit(AgentEvent::MessageEnd {
        message: Box::new(message.clone()),
    });

    Ok(message)
}

/// Fail all tool calls from a truncated assistant message.
///
/// When the response hits the output token limit, streamed tool-call arguments
/// may be incomplete. We fail them all and ask the model to re-issue.
fn fail_tool_calls_from_truncated(
    tool_calls: &[(&str, &str, serde_json::Value)],
    sink: &(dyn EventSink + Send + Sync),
) -> (Vec<crate::tool::ExecutedToolCall>, Vec<AgentMessage>) {
    let mut executed = Vec::with_capacity(tool_calls.len());
    let mut messages = Vec::with_capacity(tool_calls.len());

    for (id, name, _args) in tool_calls {
        sink.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
        });

        let result = AgentToolResult::error(format!(
            "Tool call \"{name}\" was not executed: the response hit the output token limit, \
             so its arguments may be truncated. Re-issue the tool call with complete arguments."
        ));

        let result_message = AgentMessage::ToolResult {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            content: result.content.clone(),
            is_error: true,
            details: None,
            timestamp: chrono::Utc::now(),
        };

        sink.emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: id.to_string(),
        });

        executed.push(crate::tool::ExecutedToolCall {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            result,
            result_message: result_message.clone(),
            blocked: false,
            block_reason: None,
        });
        messages.push(result_message);
    }

    (executed, messages)
}

/// No-op tool context for when tools don't need access to the execution env.
struct NoopToolContext;

impl crate::tool::ToolContext for NoopToolContext {
    fn env(&self) -> &dyn crate::env::ExecutionEnv {
        unimplemented!("NoopToolContext::env() — real tools need a proper context")
    }
    fn cwd(&self) -> &std::path::Path {
        std::path::Path::new(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Model, Usage};
    use std::sync::Mutex;

    struct MockStreamFn;
    struct MockSink {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl MockSink {
        fn new() -> Self {
            MockSink { events: Mutex::new(Vec::new()) }
        }
    }

    impl EventSink for MockSink {
        fn emit(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[async_trait::async_trait]
    impl StreamFn for MockStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "Hello, I'm a mock assistant.".into(),
                }],
                model: "mock".into(),
                provider: "mock".into(),
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
                timestamp: chrono::Utc::now(),
            })
        }
    }

    #[tokio::test]
    async fn test_run_loop_single_turn() {
        let sink = MockSink::new();
        let config = AgentLoopConfig::default();

        let mut context = AgentContext {
            system_prompt: "You are a test assistant.".into(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: Model {
                provider: "mock".into(),
                id: "mock".into(),
                context_window: 100_000,
                supports_thinking: false,
                metadata: Default::default(),
            },
            thinking_level: None,
            metadata: Default::default(),
        };

        let result = run_loop(
            &[AgentMessage::user("Hello")],
            &mut context,
            &config,
            None,
            Arc::new(MockStreamFn),
            &sink,
        )
        .await;

        assert!(result.is_ok());
        let messages = result.unwrap();
        assert!(!messages.is_empty());

        let events = sink.events.lock().unwrap();
        let has_agent_start = events.iter().any(|e| matches!(e, AgentEvent::AgentStart));
        let has_agent_end = events.iter().any(|e| matches!(e, AgentEvent::AgentEnd { .. }));
        assert!(has_agent_start, "expected AgentStart event");
        assert!(has_agent_end, "expected AgentEnd event");
    }
}