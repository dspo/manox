//! Stateless bare-model completion channel for the VS Code language-model
//! provider.
//!
//! Each `model_chat` command is one completion over the pi provider layer
//! (`ProviderRegistry::resolve_stream`): the wire messages and relayed tool
//! definitions build an `AgentContext`, the provider streams deltas back as
//! `model_text`/`model_thinking`/`model_tool_call` events, and
//! `model_chat_done` settles the request. Tools are never executed here —
//! VS Code runs them and returns their results on the next request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use pi::agent_loop::StreamFn;
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi::types::{
    AgentContext, AgentEvent, AgentMessage, AssistantMessageEvent, ContentBlock, Model, StopReason,
    StreamOptions,
};

use crate::actor::EventSink;

/// Default system prompt used when the request carries no system-role text.
const DEFAULT_SYSTEM_PROMPT: &str = "You are manox, a coding assistant running inside VS Code. Use the provided tools when they help answer the request.";

/// A tool definition relayed from the native chat. The model may emit a call
/// for it, but execution happens in VS Code — the actor never runs it.
pub struct RelayedTool {
    name: String,
    description: String,
    schema: Value,
}

#[async_trait::async_trait]
impl AgentTool for RelayedTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        Err(ToolError::ExecutionFailed(
            "relayed tools execute in VS Code".into(),
        ))
    }
}

/// Build the provider context from the wire `messages` and `tools` arrays.
///
/// System-role text folds into the system prompt (the native chat's system
/// message); user/assistant blocks map to `AgentMessage`s; `tool_result`
/// blocks become their own `ToolResult` messages so the provider's wire
/// translation can pair them with the matching tool call. Unknown roles and
/// block types are dropped.
pub fn build_context(model: &Model, messages: &Value, tools: &Value) -> AgentContext {
    let mut system_parts: Vec<String> = Vec::new();
    let mut agent_messages: Vec<AgentMessage> = Vec::new();

    if let Some(list) = messages.as_array() {
        for msg in list {
            let Some(role) = msg["role"].as_str() else {
                continue;
            };
            let blocks = msg["content"].as_array();
            match role {
                "system" => {
                    let text = blocks
                        .map(|blocks| {
                            blocks
                                .iter()
                                .filter_map(|b| b["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    if !text.is_empty() {
                        system_parts.push(text);
                    }
                }
                "user" => {
                    let mut content: Vec<ContentBlock> = Vec::new();
                    if let Some(blocks) = blocks {
                        for block in blocks {
                            match block["type"].as_str() {
                                Some("text") => {
                                    if let Some(text) = block["text"].as_str() {
                                        content.push(ContentBlock::Text {
                                            text: text.to_string(),
                                            signature: None,
                                        });
                                    }
                                }
                                Some("image") => {
                                    let (Some(data), Some(mime_type)) =
                                        (block["data"].as_str(), block["mimeType"].as_str())
                                    else {
                                        continue;
                                    };
                                    content.push(ContentBlock::Image {
                                        data: data.to_string(),
                                        mime_type: mime_type.to_string(),
                                    });
                                }
                                Some("tool_result") => {
                                    let id = block["id"].as_str().unwrap_or_default();
                                    let text = block["content"].as_str().unwrap_or_default();
                                    agent_messages.push(AgentMessage::ToolResult {
                                        tool_call_id: id.to_string(),
                                        tool_name: String::new(),
                                        content: vec![ContentBlock::Text {
                                            text: text.to_string(),
                                            signature: None,
                                        }],
                                        is_error: false,
                                        details: None,
                                        usage: None,
                                        added_tool_names: None,
                                        timestamp: Utc::now(),
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    if !content.is_empty() {
                        agent_messages.push(AgentMessage::User {
                            content,
                            timestamp: Utc::now(),
                        });
                    }
                }
                "assistant" => {
                    let mut content: Vec<ContentBlock> = Vec::new();
                    if let Some(blocks) = blocks {
                        for block in blocks {
                            match block["type"].as_str() {
                                Some("text") => {
                                    if let Some(text) = block["text"].as_str() {
                                        content.push(ContentBlock::Text {
                                            text: text.to_string(),
                                            signature: None,
                                        });
                                    }
                                }
                                Some("thinking") => {
                                    if let Some(text) = block["text"].as_str() {
                                        content.push(ContentBlock::Thinking {
                                            thinking: text.to_string(),
                                            signature: None,
                                            redacted: None,
                                        });
                                    }
                                }
                                Some("tool_call") => {
                                    let (Some(id), Some(name), Some(input)) = (
                                        block["id"].as_str(),
                                        block["name"].as_str(),
                                        block.get("input"),
                                    ) else {
                                        continue;
                                    };
                                    content.push(ContentBlock::ToolUse {
                                        id: id.to_string(),
                                        name: name.to_string(),
                                        input: input.clone(),
                                        thought_signature: None,
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    if !content.is_empty() {
                        agent_messages.push(AgentMessage::Assistant {
                            content,
                            model: model.id.clone(),
                            provider: model.provider.clone(),
                            api: model.api.clone(),
                            response_model: None,
                            response_id: None,
                            diagnostics: None,
                            stop_reason: Some(StopReason::Stop),
                            raw_stop_reason: None,
                            usage: Box::default(),
                            error_message: None,
                            timestamp: Utc::now(),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    let system_prompt = if system_parts.is_empty() {
        DEFAULT_SYSTEM_PROMPT.to_string()
    } else {
        system_parts.join("\n\n")
    };

    let relayed: Vec<Arc<dyn AgentTool>> = tools
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|t| {
                    let (Some(name), Some(description)) =
                        (t["name"].as_str(), t["description"].as_str())
                    else {
                        return None;
                    };
                    Some(Arc::new(RelayedTool {
                        name: name.to_string(),
                        description: description.to_string(),
                        schema: t.get("inputSchema").cloned().unwrap_or_else(|| json!({})),
                    }) as Arc<dyn AgentTool>)
                })
                .collect()
        })
        .unwrap_or_default();

    AgentContext {
        system_prompt,
        messages: agent_messages,
        tools: relayed.into(),
        model: model.clone(),
        thinking_level: None,
        cache_retention: Default::default(),
        session_id: None,
        stream_options: StreamOptions {
            max_tokens: Some(model.max_tokens),
            ..Default::default()
        },
        metadata: Default::default(),
    }
}

/// Emit one relayed delta as a wire event.
fn forward(sink: &EventSink, request_id: &str, ev: AgentEvent) {
    let AgentEvent::MessageUpdate {
        assistant_message_event,
        ..
    } = ev
    else {
        return;
    };
    match assistant_message_event {
        AssistantMessageEvent::TextDelta { delta, .. } => {
            sink.emit(
                json!({"type": "model_text", "requestId": request_id, "text": delta}).to_string(),
            );
        }
        AssistantMessageEvent::ThinkingDelta { delta, .. } => {
            sink.emit(
                json!({"type": "model_thinking", "requestId": request_id, "text": delta})
                    .to_string(),
            );
        }
        AssistantMessageEvent::ToolCallEnd {
            tool_call: ContentBlock::ToolUse {
                id, name, input, ..
            },
            ..
        } => {
            sink.emit(
                json!({
                    "type": "model_tool_call",
                    "requestId": request_id,
                    "id": id,
                    "name": name,
                    "input": input,
                })
                .to_string(),
            );
        }
        _ => {}
    }
}

/// The wire stop label for a settled assistant message; `None` for every
/// other message shape and for stop reasons the host does not need to
/// distinguish (plain stop, length cutoff).
fn stop_reason_str(message: &AgentMessage) -> Option<&'static str> {
    match message {
        AgentMessage::Assistant {
            stop_reason: Some(StopReason::ToolUse),
            ..
        } => Some("toolUse"),
        AgentMessage::Assistant {
            stop_reason: Some(StopReason::Error),
            ..
        } => Some("error"),
        AgentMessage::Assistant {
            stop_reason: Some(StopReason::Aborted),
            ..
        } => Some("aborted"),
        _ => None,
    }
}

/// Run one completion. The stream runs on the agent's tokio runtime; deltas
/// are forwarded as wire events and `model_chat_done` settles the request.
/// The entry is registered in `cancels` so `cancel_model_chat` can abort the
/// provider stream mid-flight.
pub fn start(
    request_id: String,
    stream: Arc<dyn StreamFn>,
    ctx: AgentContext,
    sink: EventSink,
    cancels: Arc<Mutex<HashMap<String, CancellationToken>>>,
) {
    let token = CancellationToken::new();
    cancels
        .lock()
        .unwrap()
        .insert(request_id.clone(), token.clone());

    agent::runtime::handle().spawn(async move {
        let rid = request_id.clone();
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let mut stream_fut = stream.stream(&ctx, token.clone(), tx);

        let mut closed = false;
        let result = loop {
            tokio::select! {
                ev = rx.recv(), if !closed => match ev {
                    Some(ev) => forward(&sink, &rid, ev),
                    // The stream dropped its sender; only the stream future
                    // remains, so poll just that arm from here on.
                    None => closed = true,
                },
                res = &mut stream_fut => break res,
            }
        };

        // Deltas that raced the stream future's completion still count.
        while let Ok(ev) = rx.try_recv() {
            forward(&sink, &rid, ev);
        }

        match result {
            Ok(message) => {
                let stop = stop_reason_str(&message);
                sink.emit(
                    json!({"type": "model_chat_done", "requestId": rid, "stop": stop, "error": null})
                        .to_string(),
                );
            }
            Err(err) => {
                sink.emit(
                    json!({
                        "type": "model_chat_done",
                        "requestId": rid,
                        "stop": null,
                        "error": err.to_string(),
                    })
                    .to_string(),
                );
            }
        }

        cancels.lock().unwrap().remove(&request_id);
    });
}

/// Cancel an in-flight completion by request id. Removes the entry before
/// signalling so the finished task's own removal is a no-op.
pub fn cancel(cancels: &Arc<Mutex<HashMap<String, CancellationToken>>>, request_id: &str) {
    if let Some(token) = cancels.lock().unwrap().remove(request_id) {
        token.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::types::{ContentBlock, StopReason, Usage};

    fn test_model() -> Model {
        Model {
            provider: "anthropic".into(),
            api: "anthropic".into(),
            id: "claude-sonnet".into(),
            context_window: 200_000,
            max_tokens: 8_192,
            thinking: Default::default(),
            metadata: Default::default(),
        }
    }

    #[test]
    fn system_joins_and_defaults() {
        let model = test_model();
        let messages = json!([
            {"role": "system", "content": [{"type": "text", "text": "Be terse."}]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        ]);
        let ctx = build_context(&model, &messages, &json!([]));
        assert_eq!(ctx.system_prompt, "Be terse.");
        assert_eq!(ctx.messages.len(), 1);

        let empty = build_context(&model, &json!([]), &json!([]));
        assert!(empty.system_prompt.contains("coding assistant"));
    }

    #[test]
    fn user_text_and_image_join_one_message() {
        let model = test_model();
        let messages = json!([
            {"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"}
            ]}
        ]);
        let ctx = build_context(&model, &messages, &json!([]));
        assert_eq!(ctx.messages.len(), 1);
        match &ctx.messages[0] {
            AgentMessage::User { content, .. } => {
                assert_eq!(content.len(), 2);
                assert!(matches!(
                    content[0],
                    ContentBlock::Text { ref text, .. } if text == "look"
                ));
                assert!(matches!(
                    content[1],
                    ContentBlock::Image { ref mime_type, .. } if mime_type == "image/png"
                ));
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_becomes_its_own_message() {
        let model = test_model();
        let messages = json!([
            {"role": "user", "content": [
                {"type": "text", "text": "now what"},
                {"type": "tool_result", "id": "t1", "content": "42"}
            ]}
        ]);
        let ctx = build_context(&model, &messages, &json!([]));
        assert_eq!(ctx.messages.len(), 2);
        match &ctx.messages[0] {
            AgentMessage::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                assert_eq!(tool_call_id, "t1");
                assert!(matches!(
                    content[0],
                    ContentBlock::Text { ref text, .. } if text == "42"
                ));
            }
            other => panic!("expected tool result first, got {other:?}"),
        }
        assert!(matches!(ctx.messages[1], AgentMessage::User { .. }));
    }

    #[test]
    fn assistant_carries_required_fields() {
        let model = test_model();
        let messages = json!([
            {"role": "assistant", "content": [
                {"type": "thinking", "text": "hmm"},
                {"type": "text", "text": "done"},
                {"type": "tool_call", "id": "t2", "name": "readFile", "input": {"path": "a.txt"}}
            ]}
        ]);
        let ctx = build_context(&model, &messages, &json!([]));
        assert_eq!(ctx.messages.len(), 1);
        match &ctx.messages[0] {
            AgentMessage::Assistant {
                content,
                model: m,
                provider,
                api,
                stop_reason,
                usage,
                ..
            } => {
                assert_eq!(m, "claude-sonnet");
                assert_eq!(provider, "anthropic");
                assert_eq!(api, "anthropic");
                assert_eq!(stop_reason, &Some(StopReason::Stop));
                assert!(matches!(
                    usage.as_ref(),
                    Usage {
                        total_tokens: 0,
                        ..
                    }
                ));
                assert_eq!(content.len(), 3);
                assert!(matches!(
                    content[2],
                    ContentBlock::ToolUse { ref name, ref input, .. }
                        if name == "readFile" && input["path"] == "a.txt"
                ));
            }
            other => panic!("expected assistant message, got {other:?}"),
        }
    }

    #[test]
    fn relayed_tools_pass_through_and_never_execute() {
        let model = test_model();
        let tools = json!([
            {"name": "listDir", "description": "List a directory", "inputSchema": {"type": "object"}}
        ]);
        let ctx = build_context(&model, &json!([]), &tools);
        assert_eq!(ctx.tools.len(), 1);
        let tool = ctx.tools[0].clone();
        assert_eq!(tool.name(), "listDir");
        assert_eq!(tool.description(), "List a directory");
        assert_eq!(tool.parameters_schema(), json!({"type": "object"}));
        assert!(tool.is_read_only());
    }

    #[test]
    fn relayed_tool_missing_schema_defaults_to_empty_object() {
        let model = test_model();
        let tools = json!([{"name": "x", "description": "y"}]);
        let ctx = build_context(&model, &json!([]), &tools);
        assert_eq!(ctx.tools.len(), 1);
        assert_eq!(ctx.tools[0].parameters_schema(), json!({}));
    }

    #[test]
    fn unknown_roles_and_blocks_are_dropped() {
        let model = test_model();
        let messages = json!([
            {"role": "mystery", "content": [{"type": "text", "text": "x"}]},
            {"role": "user", "content": [{"type": "unknown", "text": "y"}]}
        ]);
        let ctx = build_context(&model, &messages, &json!([]));
        assert!(ctx.messages.is_empty());
    }
}
