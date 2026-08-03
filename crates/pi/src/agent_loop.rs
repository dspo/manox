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

use crate::tool::{AgentToolResult, ToolContext, execute_tool_calls};
use crate::types::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, ContentBlock, Model, StopReason, Usage,
};

/// Sink for agent lifecycle events emitted during the loop.
///
/// Defined in [`crate::types`] next to [`AgentEvent`] so the tool execution
/// pipeline can emit lifecycle events without depending on the loop module.
pub use crate::types::EventSink;

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
    ///
    /// `MessageStart`/`MessageEnd` ownership is single: a stream that emits
    /// them (real providers do) owns them, and the loop only emits the ones
    /// the stream never sent. A stream must never emit `MessageEnd` for a
    /// message other than the one it returns.
    async fn stream(
        &self,
        context: &AgentContext,
        signal: CancellationToken,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error>;

    /// The wire-API shape this stream speaks (`"anthropic"` /
    /// `"openai_completions"` / `"openai_responses"`). Stamped onto a
    /// locally built terminal message so a failed/aborted turn still carries
    /// the model identity that was attempted.
    fn api(&self) -> &str {
        ""
    }
}

/// Resolves the provider runtime for a model — the consumer-pluggable seam
/// that switches protocol, endpoint, and credentials when the session model
/// changes (the TS `Model.api` discriminator picking a stream function).
/// `None` on the config keeps the run's fixed stream fn.
pub type StreamResolver =
    Arc<dyn Fn(&Model) -> Result<Arc<dyn StreamFn>, anyhow::Error> + Send + Sync>;

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
    tool_ctx: &dyn ToolContext,
    sink: &(dyn EventSink + Send + Sync),
) -> Result<Vec<AgentMessage>, anyhow::Error> {
    let signal = signal.unwrap_or_default();
    let mut new_messages: Vec<AgentMessage> = prompts.to_vec();

    // Prepend prompts to the context. Their MessageStart/MessageEnd lifecycle
    // is emitted inside the first turn (after AgentStart/TurnStart), matching
    // TS Pi's event order — a consumer never sees a prompt outside its run.
    let mut current_messages: Vec<AgentMessage> = context.messages.clone();
    current_messages.extend_from_slice(prompts);
    context.messages = current_messages;

    run_loop_inner(
        context,
        &mut new_messages,
        config,
        &signal,
        stream_fn,
        tool_ctx,
        sink,
    )
    .await?;
    Ok(new_messages)
}

/// Continue the agent loop from the current context without adding new messages.
pub async fn run_loop_continue(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: Arc<dyn StreamFn>,
    tool_ctx: &dyn ToolContext,
    sink: &(dyn EventSink + Send + Sync),
) -> Result<Vec<AgentMessage>, anyhow::Error> {
    let signal = signal.unwrap_or_default();

    if context.messages.is_empty() {
        anyhow::bail!("Cannot continue: no messages in context");
    }
    if matches!(
        context.messages.last(),
        Some(AgentMessage::Assistant { .. })
    ) {
        anyhow::bail!("Cannot continue from message role: assistant");
    }

    let mut new_messages: Vec<AgentMessage> = Vec::new();
    run_loop_inner(
        context,
        &mut new_messages,
        config,
        &signal,
        stream_fn,
        tool_ctx,
        sink,
    )
    .await?;
    Ok(new_messages)
}

/// The main loop logic shared by `run_loop` and `run_loop_continue`.
async fn run_loop_inner(
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig,
    signal: &CancellationToken,
    stream_fn: Arc<dyn StreamFn>,
    tool_ctx: &dyn ToolContext,
    sink: &(dyn EventSink + Send + Sync),
) -> Result<(), anyhow::Error> {
    sink.emit(AgentEvent::AgentStart).await?;
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

            sink.emit(AgentEvent::TurnStart).await?;
            turn_count += 1;

            // On the first turn, announce the initial prompt messages after
            // AgentStart/TurnStart so they belong to the run and turn that
            // consume them, mirroring TS Pi's event order.
            if turn_count == 1 {
                for msg in new_messages.iter() {
                    sink.emit(AgentEvent::MessageStart {
                        message: Box::new(msg.clone()),
                    })
                    .await?;
                    sink.emit(AgentEvent::MessageEnd {
                        message: Box::new(msg.clone()),
                    })
                    .await?;
                }
            }

            // Inject pending steering messages before the next assistant response.
            if !pending_messages.is_empty() {
                for msg in pending_messages.drain(..) {
                    sink.emit(AgentEvent::MessageStart {
                        message: Box::new(msg.clone()),
                    })
                    .await?;
                    sink.emit(AgentEvent::MessageEnd {
                        message: Box::new(msg.clone()),
                    })
                    .await?;
                    context.messages.push(msg.clone());
                    new_messages.push(msg);
                }
            }

            // Stream assistant response. With a resolver, the provider
            // runtime is picked per turn from the current model — a mid-run
            // model change switches protocol/endpoint for the next request. A
            // resolution failure materializes as a terminal error message
            // with the normal lifecycle, not a run-level panic.
            let message = match &config.stream_resolver {
                Some(resolver) => match resolver(&context.model) {
                    Ok(resolved) => {
                        stream_assistant_response(context, signal, resolved, config, sink).await?
                    }
                    Err(e) => {
                        let message = terminal_message(
                            context,
                            signal,
                            &context.model.api,
                            format!("failed to resolve provider runtime: {e}"),
                        );
                        sink.emit(AgentEvent::MessageStart {
                            message: Box::new(message.clone()),
                        })
                        .await?;
                        sink.emit(AgentEvent::MessageEnd {
                            message: Box::new(message.clone()),
                        })
                        .await?;
                        message
                    }
                },
                None => {
                    stream_assistant_response(context, signal, Arc::clone(&stream_fn), config, sink)
                        .await?
                }
            };

            new_messages.push(message.clone());
            context.messages.push(message.clone());

            let stop_reason = match &message {
                AgentMessage::Assistant { stop_reason, .. } => *stop_reason,
                _ => None,
            };

            // An error or abort stop reason carries no tool calls; end the
            // run without executing. Provider errors and local interrupts
            // (abort, transport error) reach here as a terminal `Error`/
            // `Aborted` message built by the stream error handler.
            if matches!(
                stop_reason,
                Some(StopReason::Error) | Some(StopReason::Aborted)
            ) {
                sink.emit(AgentEvent::TurnEnd {
                    message: Box::new(message),
                    tool_results: Vec::new(),
                })
                .await?;
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }

            // Extract tool calls and execute them.
            let tool_calls: Vec<(&str, &str, serde_json::Value)> = match &message {
                AgentMessage::Assistant { content, .. } => content
                    .iter()
                    .filter_map(|block| {
                        if let ContentBlock::ToolUse {
                            id, name, input, ..
                        } = block
                        {
                            Some((id.as_str(), name.as_str(), input.clone()))
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
                    fail_tool_calls_from_truncated(&tool_calls, sink).await?
                } else {
                    execute_tool_calls(
                        &tool_calls,
                        &context.tools,
                        signal.clone(),
                        tool_ctx,
                        config,
                        sink,
                        config.sequential_tool_execution,
                    )
                    .await?
                };

                // Check if all finalized calls want to terminate.
                let all_terminate = executed.iter().all(|ec| ec.result.terminate);

                tool_results = result_messages;
                has_more_tool_calls = !all_terminate;

                for result in &tool_results {
                    // A tool result is a settled message in its own turn; give
                    // it a matched MessageStart/MessageEnd pair like TS Pi.
                    sink.emit(AgentEvent::MessageStart {
                        message: Box::new(result.clone()),
                    })
                    .await?;
                    context.messages.push(result.clone());
                    new_messages.push(result.clone());
                    sink.emit(AgentEvent::MessageEnd {
                        message: Box::new(result.clone()),
                    })
                    .await?;
                }
            }

            sink.emit(AgentEvent::TurnEnd {
                message: Box::new(message.clone()),
                tool_results: tool_results.clone(),
            })
            .await?;

            // Apply next-turn context update.
            if let Some(ref prepare_next_turn) = config.prepare_next_turn
                && let Some(update) = prepare_next_turn().await?
            {
                context.model = update.model;
                context.thinking_level = update.thinking_level;
                if let Some(names) = update.active_tool_names {
                    context.tools = context
                        .tools
                        .iter()
                        .filter(|t| names.iter().any(|n| n == t.name()))
                        .cloned()
                        .collect();
                }
            }

            // Check early stop (TS `shouldStopAfterTurn`, after `turn_end` and
            // `prepareNextTurn`). The turn's assistant `message` is passed, not
            // the last appended tool result.
            if let Some(ref should_stop) = config.should_stop_after_turn
                && should_stop(&message, &tool_results, context, new_messages)
            {
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }

            // Check max turns.
            if let Some(max_turns) = config.max_turns
                && turn_count >= max_turns
            {
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
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
    })
    .await?;
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
    config: &AgentLoopConfig,
    sink: &(dyn EventSink + Send + Sync),
) -> Result<AgentMessage, anyhow::Error> {
    // Fire the provider-request hook before the stream starts. A handler may
    // return a mutated context; that mutated context is what the provider sees.
    let ctx = match &config.before_provider_request {
        Some(before) => before(context),
        None => context.clone(),
    };

    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);

    // Clone what the spawned task needs.
    let sig = signal.clone();

    // The wire-API shape, captured before the Arc moves into the spawn so a
    // locally built terminal message still stamps the attempted model identity.
    let api = stream_fn.api().to_string();

    // Spawn the stream function so it can send events concurrently.
    let stream_handle = tokio::spawn(async move { stream_fn.stream(&ctx, sig, event_tx).await });

    // Accumulate streaming state on the receiver side. The stream may emit
    // its own MessageStart/MessageEnd; the loop only backstops the lifecycle
    // events the stream never sent, so exactly one of each reaches the sink
    // per assistant message. The latest partial assistant snapshot is kept so
    // a provider failure is materialized from the text that already streamed
    // rather than an empty shell — TS marks the same partial message as an
    // error, preserving content and usage.
    let mut first = true;
    let mut saw_end = false;
    let mut last_assistant: Option<AgentMessage> = None;

    // Process events as they arrive from the channel.
    // The channel closes when the sender is dropped (stream completes or errors).
    while let Some(event) = event_rx.recv().await {
        match &event {
            AgentEvent::MessageStart { message } => {
                first = false;
                last_assistant = Some((**message).clone());
            }
            AgentEvent::MessageUpdate { message, .. } => {
                last_assistant = Some((**message).clone());
            }
            AgentEvent::MessageEnd { .. } => {
                saw_end = true;
            }
            _ => {}
        }
        sink.emit(event).await?;
    }

    // Wait for the stream to complete. A provider error or an abort never
    // propagates as `Result::Err` — it is materialized as a terminal assistant
    // message with `stop_reason = Error | Aborted` so consumers learn of the
    // failure from the event stream and the run can close out cleanly.
    let message = match stream_handle.await {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => {
            terminal_message_from_partial(last_assistant, context, signal, &api, e.to_string())
        }
        Err(join_err) => terminal_message_from_partial(
            last_assistant,
            context,
            signal,
            &api,
            format!("stream task failed: {join_err}"),
        ),
    };

    // Backstop only the lifecycle events the stream never sent. A provider
    // that emitted its own MessageEnd owns it — emitting another here would
    // append the assistant message to reducers twice. A stream that failed
    // before closing the lifecycle gets its terminal message announced now.
    if first {
        sink.emit(AgentEvent::MessageStart {
            message: Box::new(message.clone()),
        })
        .await?;
    }
    if !saw_end {
        sink.emit(AgentEvent::MessageEnd {
            message: Box::new(message.clone()),
        })
        .await?;
    }

    Ok(message)
}

/// Build a terminal assistant message for a provider error or abort.
///
/// `Aborted` when the run was cancelled, `Error` otherwise — matching the TS
/// Pi terminal stop reasons. The message carries no content and an
/// `error_message`, but keeps the model identity (`model`/`provider`/`api`)
/// of the turn that was attempted, so a failed turn is still attributable to
/// the model that was running it.
fn terminal_message(
    context: &AgentContext,
    signal: &CancellationToken,
    api: &str,
    error_message: String,
) -> AgentMessage {
    let stop_reason = if signal.is_cancelled() {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    AgentMessage::Assistant {
        content: Vec::new(),
        model: context.model.id.clone(),
        provider: context.model.provider.clone(),
        api: api.to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        raw_stop_reason: None,
        stop_reason: Some(stop_reason),
        usage: Box::new(Usage::default()),
        error_message: Some(error_message),
        timestamp: chrono::Utc::now(),
    }
}

/// Terminal error for a provider failure that struck mid-stream: when a
/// partial assistant already streamed, the failure is materialized from that
/// snapshot — same content, usage, response identity, api, and timestamp —
/// with only the stop reason and error message overwritten, mirroring TS's
/// catch which marks the in-flight `output` in place as `stopReason: error`.
/// Falls back to an empty terminal message when the stream failed before
/// emitting anything.
fn terminal_message_from_partial(
    partial: Option<AgentMessage>,
    context: &AgentContext,
    signal: &CancellationToken,
    api: &str,
    error_message: String,
) -> AgentMessage {
    let stop_reason = if signal.is_cancelled() {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    match partial {
        Some(AgentMessage::Assistant {
            content,
            model,
            provider,
            api,
            response_model,
            response_id,
            diagnostics,
            raw_stop_reason,
            usage,
            timestamp,
            ..
        }) => AgentMessage::Assistant {
            content,
            model,
            provider,
            api,
            response_model,
            response_id,
            diagnostics,
            raw_stop_reason,
            stop_reason: Some(stop_reason),
            usage,
            error_message: Some(error_message),
            timestamp,
        },
        _ => terminal_message(context, signal, api, error_message),
    }
}

/// Fail all tool calls from a truncated assistant message.
///
/// When the response hits the output token limit, streamed tool-call arguments
/// may be incomplete. We fail them all and ask the model to re-issue.
async fn fail_tool_calls_from_truncated(
    tool_calls: &[(&str, &str, serde_json::Value)],
    sink: &(dyn EventSink + Send + Sync),
) -> Result<(Vec<crate::tool::ExecutedToolCall>, Vec<AgentMessage>), anyhow::Error> {
    let mut executed = Vec::with_capacity(tool_calls.len());
    let mut messages = Vec::with_capacity(tool_calls.len());

    for (id, name, args) in tool_calls {
        sink.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            arguments: args.clone(),
        })
        .await?;

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
            usage: None,
            added_tool_names: None,
            timestamp: chrono::Utc::now(),
        };

        sink.emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            result: result.clone(),
            is_error: true,
        })
        .await?;

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

    Ok((executed, messages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ExecutionEnv;
    use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError, ToolState};
    use crate::types::{Model, ThinkingKind, Usage};
    use serde_json::Value as JsonValue;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct MockSink {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl MockSink {
        fn new() -> Self {
            MockSink {
                events: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl EventSink for MockSink {
        async fn emit(&self, event: AgentEvent) -> Result<(), anyhow::Error> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    // ── Test tool context ─────────────────────────────────────────────────────
    //
    // Loop tests drive the state machine, not the filesystem; tools under test
    // (e.g. EchoTool) ignore `ctx`, so a stub env is sufficient. Production
    // code uses `LocalToolContext` (see `tool.rs`).

    struct TestEnv;

    #[async_trait::async_trait]
    impl ExecutionEnv for TestEnv {
        fn cwd(&self) -> &Path {
            Path::new("/test")
        }
        async fn absolute_path(&self, path: &Path) -> Result<PathBuf, crate::env::FileError> {
            Ok(path.to_path_buf())
        }
        fn join_path(&self, parts: &[&str]) -> PathBuf {
            parts.iter().collect()
        }
        async fn read_file(
            &self,
            _path: &Path,
            _offset: Option<usize>,
            _limit: Option<usize>,
        ) -> Result<String, crate::env::FileError> {
            Ok(String::new())
        }
        async fn write_file(
            &self,
            _path: &Path,
            _content: &str,
        ) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn exists(&self, _path: &Path) -> Result<bool, crate::env::FileError> {
            Ok(false)
        }
        async fn file_info(
            &self,
            _path: &Path,
        ) -> Result<crate::env::FileInfo, crate::env::FileError> {
            unreachable!("TestEnv fs ops are not exercised by loop tests")
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> Result<Vec<crate::env::FileInfo>, crate::env::FileError> {
            Ok(vec![])
        }
        async fn create_dir(&self, _path: &Path) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn remove(&self, _path: &Path) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn exec(
            &self,
            _command: &str,
            _timeout: std::time::Duration,
            _signal: CancellationToken,
        ) -> Result<crate::env::CommandResult, crate::env::ExecutionError> {
            Ok(crate::env::CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            })
        }
    }

    struct TestToolContext {
        state: ToolState,
    }

    impl TestToolContext {
        fn new() -> Self {
            Self {
                state: ToolState::new(),
            }
        }
    }

    impl ToolContext for TestToolContext {
        fn env(&self) -> &dyn ExecutionEnv {
            &TestEnv
        }
        fn cwd(&self) -> &Path {
            Path::new("/test")
        }
        fn tool_state(&self) -> &ToolState {
            &self.state
        }
    }

    // ── Simple mock: always returns a text response ──────────────────────────

    struct MockStreamFn;

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
                    signature: None,
                }],
                model: "mock".into(),
                provider: "mock".into(),
                api: "mock".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(StopReason::Stop),
                usage: Box::new(Usage::default()),
                error_message: None,
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
            tools: Arc::from(vec![]),
            model: Model {
                provider: "mock".into(),
                id: "mock".into(),
                api: "test".into(),
                context_window: 100_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        };

        let result = run_loop(
            &[AgentMessage::user("Hello")],
            &mut context,
            &config,
            None,
            Arc::new(MockStreamFn),
            &TestToolContext::new(),
            &sink,
        )
        .await;

        assert!(result.is_ok());
        let messages = result.unwrap();
        assert!(!messages.is_empty());

        let events = sink.events.lock().unwrap();
        let has_agent_start = events.iter().any(|e| matches!(e, AgentEvent::AgentStart));
        let has_agent_end = events
            .iter()
            .any(|e| matches!(e, AgentEvent::AgentEnd { .. }));
        assert!(has_agent_start, "expected AgentStart event");
        assert!(has_agent_end, "expected AgentEnd event");
    }

    // Cloning the context (notably across the `tokio::spawn` boundary in the
    // stream path) must preserve the mounted tool list — otherwise the
    // provider would see an empty `tools` array and never emit tool_use.
    #[test]
    fn agent_context_clone_preserves_tools() {
        let ctx = AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Arc::from(vec![Arc::new(EchoTool) as Arc<dyn AgentTool>]),
            model: Model {
                provider: "mock".into(),
                id: "mock".into(),
                api: "test".into(),
                context_window: 100_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        };
        let cloned = ctx.clone();
        assert_eq!(cloned.tools.len(), 1, "clone must preserve mounted tools");
        assert_eq!(
            cloned.tools[0].name(),
            "echo",
            "clone must preserve the same tool identity"
        );
    }

    // ── Stateful mock: returns different messages based on call count ─────────

    struct StatefulMockStreamFn {
        responses: Mutex<Vec<AgentMessage>>,
    }

    impl StatefulMockStreamFn {
        fn new(responses: Vec<AgentMessage>) -> Self {
            StatefulMockStreamFn {
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait::async_trait]
    impl StreamFn for StatefulMockStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            let mut guard = self.responses.lock().unwrap();
            if guard.is_empty() {
                // Fallback: return a default end-turn response.
                return Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                        signature: None,
                    }],
                    model: "mock".into(),
                    provider: "mock".into(),
                    api: "mock".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(StopReason::Stop),
                    usage: Box::new(Usage::default()),
                    error_message: None,
                    timestamp: chrono::Utc::now(),
                });
            }
            Ok(guard.remove(0))
        }
    }

    // ── Mock tool that echoes arguments ──────────────────────────────────────

    struct EchoTool;

    #[async_trait::async_trait]
    impl AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes input"
        }
        fn is_read_only(&self) -> bool {
            true
        }
        fn parameters_schema(&self) -> JsonValue {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            params: JsonValue,
            _signal: CancellationToken,
            _ctx: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            let msg = params["message"].as_str().unwrap_or("no message");
            Ok(AgentToolResult::text(msg))
        }
    }

    // ── Multi-turn tool call loop ────────────────────────────────────────────

    #[tokio::test]
    async fn test_run_loop_multi_turn_tool_calls() {
        let sink = MockSink::new();

        // Set a max turns to prevent infinite loops.
        let config = AgentLoopConfig {
            max_turns: Some(10),
            ..Default::default()
        };

        let mut context = AgentContext {
            system_prompt: "You are a test assistant.".into(),
            messages: Vec::new(),
            tools: Arc::from(vec![Arc::new(EchoTool) as Arc<dyn AgentTool>]),
            model: Model {
                provider: "mock".into(),
                id: "mock".into(),
                api: "test".into(),
                context_window: 100_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        };

        // First response: a tool call.
        let tool_call_msg = AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "echo".into(),
                input: serde_json::json!({"message": "hello"}),
                thought_signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::ToolUse),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };

        // Second response: text after tool result.
        let text_msg = AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "Tool executed successfully.".into(),
                signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };

        let stream_fn = Arc::new(StatefulMockStreamFn::new(vec![tool_call_msg, text_msg]));

        let result = run_loop(
            &[AgentMessage::user("echo hello")],
            &mut context,
            &config,
            None,
            stream_fn,
            &TestToolContext::new(),
            &sink,
        )
        .await;

        assert!(result.is_ok());
        let messages = result.unwrap();

        // Should have: user, assistant(tool_call), tool_result, assistant(text)
        let has_tool_result = messages
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult { .. }));
        assert!(has_tool_result, "expected a tool result message");

        let has_tool_call = messages.iter().any(|m| {
            matches!(m, AgentMessage::Assistant { stop_reason, .. } if *stop_reason == Some(StopReason::ToolUse))
        });
        assert!(has_tool_call, "expected a tool call message");

        let events = sink.events.lock().unwrap();
        let turn_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnStart))
            .count();
        assert!(
            turn_count >= 2,
            "expected at least 2 turns, got {turn_count}"
        );
    }

    // ── should_stop_after_turn: TS `shouldStopAfterTurn` graceful stop ────────

    // The hook fires after `turn_end` + `prepareNextTurn`, receives the turn's
    // assistant `message` (not the last appended tool result), and — on true —
    // ends the run before the next LLM call.
    #[tokio::test]
    async fn should_stop_after_turn_receives_assistant_message_and_stops_run() {
        let sink = MockSink::new();
        let captured: Arc<Mutex<Option<AgentMessage>>> = Arc::new(Mutex::new(None));
        let captured_hook = Arc::clone(&captured);
        let config = AgentLoopConfig {
            max_turns: Some(10),
            should_stop_after_turn: Some(Box::new(
                move |msg: &AgentMessage,
                      _results: &[AgentMessage],
                      _ctx: &AgentContext,
                      _new: &[AgentMessage]| {
                    *captured_hook.lock().unwrap() = Some(msg.clone());
                    true
                },
            )),
            ..Default::default()
        };

        let mut context = AgentContext {
            system_prompt: "You are a test assistant.".into(),
            messages: Vec::new(),
            tools: Arc::from(vec![Arc::new(EchoTool) as Arc<dyn AgentTool>]),
            model: Model {
                provider: "mock".into(),
                id: "mock".into(),
                api: "test".into(),
                context_window: 100_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        };

        let tool_call_msg = AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "echo".into(),
                input: serde_json::json!({"message": "hello"}),
                thought_signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::ToolUse),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        let text_msg = AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "Tool executed successfully.".into(),
                signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        // Two responses queued; should_stop must cut the run after the first so
        // text_msg is never consumed.
        let stream_fn = Arc::new(StatefulMockStreamFn::new(vec![tool_call_msg, text_msg]));

        let result = run_loop(
            &[AgentMessage::user("echo hello")],
            &mut context,
            &config,
            None,
            stream_fn,
            &TestToolContext::new(),
            &sink,
        )
        .await;
        assert!(result.is_ok());

        let captured_msg = captured
            .lock()
            .unwrap()
            .clone()
            .expect("should_stop_after_turn fired");
        assert!(
            matches!(
                captured_msg,
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::ToolUse),
                    ..
                }
            ),
            "should_stop received the turn's assistant message, not a tool result: {captured_msg:?}"
        );

        let events = sink.events.lock().unwrap();
        let turn_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnStart))
            .count();
        assert_eq!(
            turn_count, 1,
            "should_stop ends the run before a second turn"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        );
    }

    // A false return lets the run proceed to the next turn (the tool loop
    // continues as if no hook were set).
    #[tokio::test]
    async fn should_stop_after_turn_false_lets_run_continue() {
        let sink = MockSink::new();
        let fired = Arc::new(Mutex::new(0u32));
        let fired_hook = Arc::clone(&fired);
        let config = AgentLoopConfig {
            max_turns: Some(10),
            should_stop_after_turn: Some(Box::new(
                move |_msg: &AgentMessage,
                      _results: &[AgentMessage],
                      _ctx: &AgentContext,
                      _new: &[AgentMessage]| {
                    *fired_hook.lock().unwrap() += 1;
                    false
                },
            )),
            ..Default::default()
        };

        let mut context = AgentContext {
            system_prompt: "You are a test assistant.".into(),
            messages: Vec::new(),
            tools: Arc::from(vec![Arc::new(EchoTool) as Arc<dyn AgentTool>]),
            model: Model {
                provider: "mock".into(),
                id: "mock".into(),
                api: "test".into(),
                context_window: 100_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        };

        let tool_call_msg = AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "echo".into(),
                input: serde_json::json!({"message": "hello"}),
                thought_signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::ToolUse),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        let text_msg = AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "done".into(),
                signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        let stream_fn = Arc::new(StatefulMockStreamFn::new(vec![tool_call_msg, text_msg]));

        let result = run_loop(
            &[AgentMessage::user("echo hello")],
            &mut context,
            &config,
            None,
            stream_fn,
            &TestToolContext::new(),
            &sink,
        )
        .await;
        assert!(result.is_ok());

        assert_eq!(
            *fired.lock().unwrap(),
            2,
            "hook fires once per turn across 2 turns"
        );
        let events = sink.events.lock().unwrap();
        let turn_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnStart))
            .count();
        assert_eq!(
            turn_count, 2,
            "a false return lets the run reach the second turn"
        );
    }

    // ── Truncation safety: stop_reason == Length fails all tool calls ────────

    #[tokio::test]
    async fn test_run_loop_truncation_safety() {
        let sink = MockSink::new();
        let config = AgentLoopConfig::default();

        let mut context = AgentContext {
            system_prompt: "You are a test assistant.".into(),
            messages: Vec::new(),
            tools: Arc::from(vec![Arc::new(EchoTool) as Arc<dyn AgentTool>]),
            model: Model {
                provider: "mock".into(),
                id: "mock".into(),
                api: "test".into(),
                context_window: 100_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        };

        // Assistant message with stop_reason == Length and a tool call.
        // The tool call should be failed because the response was truncated.
        let truncated_msg = AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "call_truncated".into(),
                name: "echo".into(),
                input: serde_json::json!({"message": "incomplete"}),
                thought_signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Length),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };

        let stream_fn = Arc::new(StatefulMockStreamFn::new(vec![truncated_msg]));

        let result = run_loop(
            &[AgentMessage::user("test")],
            &mut context,
            &config,
            None,
            stream_fn,
            &TestToolContext::new(),
            &sink,
        )
        .await;

        assert!(result.is_ok());
        let messages = result.unwrap();

        // The tool call should have been failed with an error result.
        let tool_result = messages
            .iter()
            .find(|m| matches!(m, AgentMessage::ToolResult { .. }));
        assert!(tool_result.is_some(), "expected a tool result message");

        if let Some(AgentMessage::ToolResult {
            is_error, content, ..
        }) = tool_result
        {
            assert!(is_error, "truncated tool call should be an error");
            let text = content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::Text { text, .. } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(
                text.contains("output token limit") || text.contains("truncated"),
                "error should mention truncation, got: {text}"
            );
        }
    }

    // ── Abort during loop ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_run_loop_abort() {
        let sink = MockSink::new();
        let config = AgentLoopConfig::default();
        let signal = CancellationToken::new();

        let mut context = AgentContext {
            system_prompt: "You are a test assistant.".into(),
            messages: Vec::new(),
            tools: Arc::from(vec![]),
            model: Model {
                provider: "mock".into(),
                id: "mock".into(),
                api: "test".into(),
                context_window: 100_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        };

        // Cancel before any stream — the loop should exit immediately.
        signal.cancel();

        let result = run_loop(
            &[AgentMessage::user("test")],
            &mut context,
            &config,
            Some(signal),
            Arc::new(MockStreamFn),
            &TestToolContext::new(),
            &sink,
        )
        .await;

        // Should complete without error (loop exits early).
        assert!(result.is_ok());
        let messages = result.unwrap();
        // The user message was prepended, but no assistant message should be produced.
        let assistant_count = messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::Assistant { .. }))
            .count();
        assert_eq!(
            assistant_count, 0,
            "aborted loop should not produce assistant messages"
        );
    }

    // ── Follow-up messages: outer loop continues ─────────────────────────────

    #[tokio::test]
    async fn test_run_loop_follow_up() {
        let sink = MockSink::new();

        use std::sync::atomic::{AtomicUsize, Ordering};

        let follow_up_count = Arc::new(AtomicUsize::new(0));
        let follow_up_count_clone = Arc::clone(&follow_up_count);

        let mut config = AgentLoopConfig {
            max_turns: Some(10),
            ..Default::default()
        };

        // Provide one follow-up message, then none.
        let follow_up_provided = Arc::new(AtomicUsize::new(0));
        let follow_up_provided_clone = Arc::clone(&follow_up_provided);

        config.get_follow_up_messages = Some(Box::new(move || {
            let count = follow_up_provided_clone.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                follow_up_count_clone.fetch_add(1, Ordering::SeqCst);
                vec![AgentMessage::user("follow-up message")]
            } else {
                Vec::new()
            }
        }));

        let mut context = AgentContext {
            system_prompt: "You are a test assistant.".into(),
            messages: Vec::new(),
            tools: Arc::from(vec![]),
            model: Model {
                provider: "mock".into(),
                id: "mock".into(),
                api: "test".into(),
                context_window: 100_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        };

        let stream_fn = Arc::new(MockStreamFn);

        let result = run_loop(
            &[AgentMessage::user("initial")],
            &mut context,
            &config,
            None,
            stream_fn,
            &TestToolContext::new(),
            &sink,
        )
        .await;

        assert!(result.is_ok());
        let messages = result.unwrap();

        // Should have: user(initial), assistant, user(follow-up), assistant
        let user_count = messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::User { .. }))
            .count();
        assert_eq!(user_count, 2, "expected 2 user messages, got {user_count}");

        let assistant_count = messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::Assistant { .. }))
            .count();
        assert_eq!(
            assistant_count, 2,
            "expected 2 assistant messages, got {assistant_count}"
        );

        assert_eq!(
            follow_up_count.load(Ordering::SeqCst),
            1,
            "follow-up should have been triggered once"
        );
    }

    // ── #365: initial prompt lifecycle ────────────────────────────────────────

    fn minimal_context() -> AgentContext {
        AgentContext {
            system_prompt: "You are a test assistant.".into(),
            messages: Vec::new(),
            tools: Arc::from(vec![Arc::new(EchoTool) as Arc<dyn AgentTool>]),
            model: Model {
                provider: "mock".into(),
                id: "mock".into(),
                api: "test".into(),
                context_window: 100_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        }
    }

    #[tokio::test]
    async fn initial_prompt_emits_message_lifecycle() {
        let sink = MockSink::new();
        let config = AgentLoopConfig::default();
        let mut context = minimal_context();

        let result = run_loop(
            &[AgentMessage::user("hello")],
            &mut context,
            &config,
            None,
            Arc::new(MockStreamFn),
            &TestToolContext::new(),
            &sink,
        )
        .await;
        assert!(result.is_ok());

        let events = sink.events.lock().unwrap();
        // The initial user prompt must be announced with a matched
        // MessageStart/MessageEnd pair.
        let has_user_start = events.iter().any(|e| {
            matches!(
                e,
                AgentEvent::MessageStart { message }
                if matches!(**message, AgentMessage::User { .. })
            )
        });
        let has_user_end = events.iter().any(|e| {
            matches!(
                e,
                AgentEvent::MessageEnd { message }
                if matches!(**message, AgentMessage::User { .. })
            )
        });
        assert!(has_user_start, "initial prompt must emit MessageStart");
        assert!(has_user_end, "initial prompt must emit MessageEnd");

        // TS Pi orders the run/turn lifecycle before the user message: the
        // first MessageStart for a user prompt must follow AgentStart and the
        // first TurnStart.
        let user_start_idx = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    AgentEvent::MessageStart { message }
                        if matches!(**message, AgentMessage::User { .. })
                )
            })
            .expect("a user MessageStart");
        let agent_start_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::AgentStart))
            .expect("an AgentStart");
        let turn_start_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::TurnStart))
            .expect("a TurnStart");
        assert!(
            agent_start_idx < user_start_idx,
            "AgentStart must precede the user MessageStart"
        );
        assert!(
            turn_start_idx < user_start_idx,
            "TurnStart must precede the user MessageStart"
        );
    }

    // ── #366: before/after tool call hooks ────────────────────────────────────

    #[tokio::test]
    async fn before_tool_call_blocks_execution() {
        let sink = MockSink::new();
        // Block the "echo" tool with a reason.
        let config = AgentLoopConfig {
            before_tool_call: Some(Box::new(|_id, name, _args| {
                if name == "echo" {
                    Some("blocked by test".into())
                } else {
                    None
                }
            })),
            max_turns: Some(2),
            ..Default::default()
        };
        let mut context = minimal_context();

        let tool_call_msg = AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "echo".into(),
                input: serde_json::json!({"message": "hello"}),
                thought_signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::ToolUse),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        let stream_fn = Arc::new(StatefulMockStreamFn::new(vec![tool_call_msg]));

        run_loop(
            &[AgentMessage::user("echo hello")],
            &mut context,
            &config,
            None,
            stream_fn,
            &TestToolContext::new(),
            &sink,
        )
        .await
        .unwrap();

        // The blocked tool result is appended to the transcript as an error.
        let blocked_result = context.messages.iter().find_map(|m| match m {
            AgentMessage::ToolResult {
                content, is_error, ..
            } if content.iter().any(
                |b| matches!(b, ContentBlock::Text { text, .. } if text.contains("blocked")),
            ) =>
            {
                Some(*is_error)
            }
            _ => None,
        });
        assert_eq!(
            blocked_result,
            Some(true),
            "blocked tool call must produce an error tool_result"
        );

        // Start/End must still be paired so the UI sees a clean boundary.
        let events = sink.events.lock().unwrap();
        let starts = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolExecutionStart { .. }))
            .count();
        let ends = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
            .count();
        assert_eq!(starts, 1, "one ToolExecutionStart for the blocked call");
        assert_eq!(ends, 1, "one ToolExecutionEnd for the blocked call");
    }

    #[tokio::test]
    async fn after_tool_call_patches_result() {
        let sink = MockSink::new();
        let config = AgentLoopConfig {
            after_tool_call: Some(Box::new(|r: &AgentToolResult| {
                let mut patched = r.clone();
                if let Some(crate::types::ContentBlock::Text { text, .. }) =
                    patched.content.get_mut(0)
                {
                    *text = "patched".into();
                }
                patched
            })),
            max_turns: Some(2),
            ..Default::default()
        };
        let mut context = minimal_context();

        let tool_call_msg = AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "echo".into(),
                input: serde_json::json!({"message": "hello"}),
                thought_signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::ToolUse),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        let stream_fn = Arc::new(StatefulMockStreamFn::new(vec![tool_call_msg]));

        run_loop(
            &[AgentMessage::user("echo hello")],
            &mut context,
            &config,
            None,
            stream_fn,
            &TestToolContext::new(),
            &sink,
        )
        .await
        .unwrap();

        // The patched content replaces the tool's own "hello" output.
        let patched = context.messages.iter().any(|m| {
            matches!(
                m,
                AgentMessage::ToolResult { content, .. }
                if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "patched"))
            )
        });
        assert!(
            patched,
            "after_tool_call must rewrite the tool result content"
        );
    }

    // ── #368: provider error / abort terminal message ────────────────────────

    struct ErrStreamFn;
    #[async_trait::async_trait]
    impl StreamFn for ErrStreamFn {
        fn api(&self) -> &str {
            "test_api"
        }

        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            Err(anyhow::anyhow!("provider boom"))
        }
    }

    struct AbortStreamFn;
    #[async_trait::async_trait]
    impl StreamFn for AbortStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            signal: CancellationToken,
            _event_tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            signal.cancel();
            Err(anyhow::anyhow!("aborted by mock"))
        }
    }

    #[tokio::test]
    async fn provider_error_produces_terminal_error_message() {
        let sink = MockSink::new();
        let config = AgentLoopConfig::default();
        let mut context = minimal_context();

        let result = run_loop(
            &[AgentMessage::user("hi")],
            &mut context,
            &config,
            None,
            Arc::new(ErrStreamFn),
            &TestToolContext::new(),
            &sink,
        )
        .await;

        // The run closes out cleanly (Ok), surfacing the failure as a
        // terminal assistant message rather than a propagated Err.
        let messages = result.unwrap();
        let terminal = messages.iter().find_map(|m| match m {
            AgentMessage::Assistant {
                stop_reason,
                error_message,
                model,
                provider,
                api,
                ..
            } if *stop_reason == Some(StopReason::Error) => Some((
                error_message.clone(),
                model.clone(),
                provider.clone(),
                api.clone(),
            )),
            _ => None,
        });
        let (error_message, model, provider, api) = terminal.expect("terminal Error message");
        assert_eq!(
            error_message.as_deref(),
            Some("provider boom"),
            "error must materialize as a terminal Error assistant message"
        );
        // The failed turn keeps the model identity that was attempted.
        assert_eq!(model, "mock");
        assert_eq!(provider, "mock");
        assert_eq!(api, "test_api");

        let events = sink.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnEnd { .. })),
            "error path must emit TurnEnd"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. })),
            "error path must emit AgentEnd"
        );
    }

    #[tokio::test]
    async fn abort_produces_terminal_aborted_message() {
        let sink = MockSink::new();
        let config = AgentLoopConfig::default();
        let mut context = minimal_context();

        let messages = run_loop(
            &[AgentMessage::user("hi")],
            &mut context,
            &config,
            None,
            Arc::new(AbortStreamFn),
            &TestToolContext::new(),
            &sink,
        )
        .await
        .unwrap();

        let aborted = messages.iter().any(|m| {
            matches!(
                m,
                AgentMessage::Assistant { stop_reason, .. }
                if *stop_reason == Some(StopReason::Aborted)
            )
        });
        assert!(
            aborted,
            "abort must materialize as a terminal Aborted assistant message"
        );
    }

    /// A stream that emits the full message lifecycle itself, as the real
    /// providers (Anthropic / OpenAI completions / responses) do.
    struct LifecycleStreamFn;

    #[async_trait::async_trait]
    impl StreamFn for LifecycleStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            event_tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            let message = AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "streamed".into(),
                    signature: None,
                }],
                model: "mock".into(),
                provider: "mock".into(),
                api: "mock".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(StopReason::Stop),
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            };
            let _ = event_tx
                .send(AgentEvent::MessageStart {
                    message: Box::new(message.clone()),
                })
                .await;
            let _ = event_tx
                .send(AgentEvent::MessageUpdate {
                    message: Box::new(message.clone()),
                    assistant_message_event: crate::types::AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: "streamed".into(),
                    },
                })
                .await;
            let _ = event_tx
                .send(AgentEvent::MessageEnd {
                    message: Box::new(message.clone()),
                })
                .await;
            Ok(message)
        }
    }

    #[tokio::test]
    async fn stream_owned_message_lifecycle_is_not_duplicated() {
        let sink = MockSink::new();
        let config = AgentLoopConfig::default();
        let mut context = minimal_context();

        let messages = run_loop(
            &[AgentMessage::user("hi")],
            &mut context,
            &config,
            None,
            Arc::new(LifecycleStreamFn),
            &TestToolContext::new(),
            &sink,
        )
        .await
        .unwrap();

        let assistant_count = messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::Assistant { .. }))
            .count();
        assert_eq!(assistant_count, 1, "the run returns one assistant message");

        let events = sink.events.lock().unwrap();
        let assistant_lifecycle = |kind: fn(&AgentEvent) -> bool| -> usize {
            events
                .iter()
                .filter(|e| {
                    kind(e)
                        && matches!(
                            e,
                            AgentEvent::MessageStart { message }
                            | AgentEvent::MessageEnd { message }
                            if matches!(message.as_ref(), AgentMessage::Assistant { .. })
                        )
                })
                .count()
        };
        assert_eq!(
            assistant_lifecycle(|e| matches!(e, AgentEvent::MessageStart { .. })),
            1,
            "the assistant message_start the stream sent is the only one"
        );
        assert_eq!(
            assistant_lifecycle(|e| matches!(e, AgentEvent::MessageEnd { .. })),
            1,
            "the loop must not append a second message_end after the stream's own"
        );
    }

    /// A stream that emits a partial assistant then fails mid-flight, as a
    /// provider does when the connection drops after streaming some text.
    struct PartialThenFailStreamFn {
        /// The timestamp stamped on the partial; the terminal error must keep
        /// it, like TS mutating the in-flight output in place.
        partial_timestamp: chrono::DateTime<chrono::Utc>,
    }

    #[async_trait::async_trait]
    impl StreamFn for PartialThenFailStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            event_tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            let mut message = AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "partial".into(),
                    signature: None,
                }],
                model: "mock".into(),
                provider: "mock".into(),
                api: "mock".into(),
                response_model: None,
                response_id: Some("resp_1".into()),
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: None,
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: self.partial_timestamp,
            };
            if let AgentMessage::Assistant { usage, .. } = &mut message {
                usage.output_tokens = 7;
            }
            let _ = event_tx
                .send(AgentEvent::MessageStart {
                    message: Box::new(message.clone()),
                })
                .await;
            let _ = event_tx
                .send(AgentEvent::MessageUpdate {
                    message: Box::new(message.clone()),
                    assistant_message_event: crate::types::AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: "partial".into(),
                    },
                })
                .await;
            Err(anyhow::anyhow!("connection reset"))
        }
    }

    /// A mid-stream provider failure is materialized from the partial
    /// assistant that already streamed — content, usage, response id, and
    /// timestamp survive the error, so the text the user saw does not vanish
    /// from the persisted turn (TS marks the same in-flight output as an
    /// error).
    #[tokio::test]
    async fn provider_error_keeps_streamed_partial_content() {
        let sink = MockSink::new();
        let config = AgentLoopConfig::default();
        let mut context = minimal_context();
        let partial_timestamp = chrono::DateTime::parse_from_rfc3339("2026-05-28T07:13:46.608Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let messages = run_loop(
            &[AgentMessage::user("hi")],
            &mut context,
            &config,
            None,
            Arc::new(PartialThenFailStreamFn { partial_timestamp }),
            &TestToolContext::new(),
            &sink,
        )
        .await
        .unwrap();

        let assistant = messages
            .iter()
            .find(|m| matches!(m, AgentMessage::Assistant { .. }))
            .expect("terminal assistant");
        let AgentMessage::Assistant {
            content,
            stop_reason,
            error_message,
            usage,
            response_id,
            timestamp,
            api,
            ..
        } = assistant
        else {
            panic!("expected assistant")
        };
        assert_eq!(*stop_reason, Some(StopReason::Error));
        assert!(
            error_message
                .as_deref()
                .is_some_and(|m| m.contains("connection reset")),
            "{assistant:?}"
        );
        assert!(
            matches!(&content[0], ContentBlock::Text { text, .. } if text == "partial"),
            "the streamed partial text survives the error: {assistant:?}"
        );
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(response_id.as_deref(), Some("resp_1"));
        assert_eq!(
            *timestamp, partial_timestamp,
            "the error keeps the stream's timestamp"
        );
        assert_eq!(api, "mock");
    }
}
