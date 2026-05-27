//! rig-core 0.37 适配 — 唯一接触 `rig_core::*` 的文件。
//!
//! 把 cx 的 `(provider, model, wire_api, base_url, api_key)` 转成 rig 的 client → model →
//! StreamingCompletionResponse，再把 `StreamedAssistantContent` 流转成 `CxStreamEvent`。

use std::pin::Pin;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use rig_core::OneOrMany;
use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionModel, GetTokenUsage};
use rig_core::message::{
    AssistantContent as RigAssistantContent, Message as RigMessage,
    ReasoningContent as RigReasoningContent, Text as RigText, ToolCall as RigToolCall,
    ToolFunction as RigToolFunction, ToolResult as RigToolResult, ToolResultContent,
    UserContent as RigUserContent,
};
use rig_core::providers::{anthropic, openai};
use rig_core::streaming::{
    StreamedAssistantContent, StreamingCompletionResponse, ToolCallDeltaContent,
};

use crate::WireApi;
use crate::cx_agent::config::ProviderAdapterConfig;
use crate::cx_agent::events::CxStreamEvent;
use crate::cx_agent::history::{CxContent, CxMessage};
use crate::cx_agent::provider::{CxToolDefinition, CxTurnRequest, ProviderAdapter};

pub struct RigAdapter {
    inner: RigClient,
}

enum RigClient {
    OpenAiResponses(openai::Client),
    OpenAiCompletions(openai::CompletionsClient),
    Anthropic(anthropic::Client),
}

impl RigAdapter {
    pub fn new(config: ProviderAdapterConfig) -> Result<Self> {
        let inner = match config.wire_api {
            WireApi::Responses => {
                let client = openai::Client::builder()
                    .api_key(config.api_key.as_str())
                    .base_url(config.base_url.as_str())
                    .build()
                    .map_err(|e| anyhow!("初始化 OpenAI Responses client 失败: {e}"))?;
                RigClient::OpenAiResponses(client)
            }
            WireApi::Completions => {
                let client = openai::CompletionsClient::builder()
                    .api_key(config.api_key.as_str())
                    .base_url(config.base_url.as_str())
                    .build()
                    .map_err(|e| anyhow!("初始化 OpenAI Completions client 失败: {e}"))?;
                RigClient::OpenAiCompletions(client)
            }
            WireApi::Anthropic => {
                let client = anthropic::Client::builder()
                    .api_key(config.api_key.as_str())
                    .base_url(config.base_url.as_str())
                    .build()
                    .map_err(|e| anyhow!("初始化 Anthropic client 失败: {e}"))?;
                RigClient::Anthropic(client)
            }
            WireApi::Unavailable => {
                return Err(anyhow!("wire_api=unavailable 不可用于 Cx Agent"));
            }
        };
        Ok(Self { inner })
    }
}

#[async_trait]
impl ProviderAdapter for RigAdapter {
    async fn stream_turn<'a>(
        &'a self,
        request: CxTurnRequest,
    ) -> Result<BoxStream<'a, Result<CxStreamEvent>>> {
        let model_id = request.model_id.clone();
        let max_tokens = request.max_tokens.unwrap_or(2048);

        let preamble = request.system.clone();

        match &self.inner {
            RigClient::OpenAiResponses(client) => {
                let (history, prompt) = build_rig_history(&request, false)?;
                let model = client.completion_model(model_id.as_str());
                let mut builder = model
                    .completion_request(prompt)
                    .max_tokens(max_tokens)
                    .messages(history);
                if let Some(sys) = preamble {
                    builder = builder.preamble(sys);
                }
                builder = with_tools(builder, &request.tools);
                let stream = builder
                    .stream()
                    .await
                    .map_err(|e| anyhow!("Responses stream() 失败: {e}"))?;
                Ok(map_stream(Box::pin(stream)))
            }
            RigClient::OpenAiCompletions(client) => {
                let (history, prompt) = build_rig_history(&request, false)?;
                let model = client.completion_model(model_id.as_str());
                let mut builder = model
                    .completion_request(prompt)
                    .max_tokens(max_tokens)
                    .messages(history);
                if let Some(sys) = preamble {
                    builder = builder.preamble(sys);
                }
                builder = with_tools(builder, &request.tools);
                let stream = builder
                    .stream()
                    .await
                    .map_err(|e| anyhow!("Completions stream() 失败: {e}"))?;
                Ok(map_stream(Box::pin(stream)))
            }
            RigClient::Anthropic(client) => {
                let (history, prompt) = build_rig_history(&request, true)?;
                let model = client.completion_model(model_id.as_str());
                let mut builder = model
                    .completion_request(prompt)
                    .max_tokens(max_tokens)
                    .messages(history);
                if let Some(sys) = preamble {
                    builder = builder.preamble(sys);
                }
                builder = with_tools(builder, &request.tools);
                let stream = builder
                    .stream()
                    .await
                    .map_err(|e| anyhow!("Anthropic stream() 失败: {e}"))?;
                Ok(map_stream(Box::pin(stream)))
            }
        }
    }
}

fn with_tools<M: CompletionModel>(
    mut builder: rig_core::completion::CompletionRequestBuilder<M>,
    tools: &[CxToolDefinition],
) -> rig_core::completion::CompletionRequestBuilder<M> {
    for t in tools {
        builder = builder.tool(rig_core::completion::ToolDefinition {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.input_schema.clone(),
        });
    }
    builder
}

fn build_rig_history(
    req: &CxTurnRequest,
    structured_tool_results: bool,
) -> Result<(Vec<RigMessage>, RigMessage)> {
    let mut messages: Vec<RigMessage> = Vec::with_capacity(req.history.len());
    for msg in &req.history {
        if let Some(rm) = cx_to_rig(msg, structured_tool_results)? {
            messages.push(rm);
        }
    }
    let prompt = match messages.pop() {
        Some(m) => m,
        None => RigMessage::user("(空)"),
    };
    Ok((messages, prompt))
}

fn cx_to_rig(msg: &CxMessage, structured_tool_results: bool) -> Result<Option<RigMessage>> {
    match msg {
        CxMessage::System { content } => Ok(Some(RigMessage::system(content.clone()))),
        CxMessage::User { content } => {
            let parts: Vec<RigUserContent> = content
                .iter()
                .filter_map(|c| match c {
                    CxContent::Text { text } => {
                        Some(RigUserContent::Text(RigText { text: text.clone() }))
                    }
                    CxContent::Reasoning { .. } | CxContent::ToolCall { .. } => None,
                })
                .collect();
            if parts.is_empty() {
                return Ok(None);
            }
            Ok(Some(RigMessage::User {
                content: one_or_many(parts)?,
            }))
        }
        CxMessage::Assistant { content } => {
            let mut parts: Vec<RigAssistantContent> = Vec::new();
            for c in content {
                match c {
                    CxContent::Text { text } => {
                        parts.push(RigAssistantContent::Text(RigText { text: text.clone() }));
                    }
                    CxContent::Reasoning { text } => {
                        parts.push(RigAssistantContent::Reasoning(
                            rig_core::message::Reasoning::new(text),
                        ));
                    }
                    CxContent::ToolCall {
                        id,
                        call_id,
                        name,
                        arguments,
                    } => {
                        parts.push(RigAssistantContent::ToolCall(RigToolCall {
                            id: id.clone(),
                            call_id: call_id.clone(),
                            function: RigToolFunction {
                                name: name.clone(),
                                arguments: arguments.clone(),
                            },
                            signature: None,
                            additional_params: None,
                        }));
                    }
                }
            }
            if parts.is_empty() {
                return Ok(None);
            }
            Ok(Some(RigMessage::Assistant {
                id: None,
                content: one_or_many(parts)?,
            }))
        }
        CxMessage::ToolResult {
            call_id,
            name,
            content,
            is_error,
            ..
        } => {
            if structured_tool_results {
                Ok(Some(RigMessage::User {
                    content: OneOrMany::one(RigUserContent::ToolResult(RigToolResult {
                        id: call_id.clone(),
                        call_id: Some(call_id.clone()),
                        content: OneOrMany::one(ToolResultContent::Text(RigText {
                            text: content.clone(),
                        })),
                    })),
                }))
            } else {
                let tool_name = name.as_deref().unwrap_or("tool");
                let status = if *is_error { "error" } else { "result" };
                Ok(Some(RigMessage::user(format!(
                    "Tool `{tool_name}` ({call_id}) {status}:\n{content}"
                ))))
            }
        }
    }
}

fn one_or_many<T>(items: Vec<T>) -> Result<OneOrMany<T>>
where
    T: Clone,
{
    OneOrMany::many(items).map_err(|e| anyhow!("空内容列表: {e}"))
}

struct AdapterStreamState<R>
where
    R: Clone + Unpin + GetTokenUsage,
{
    stream: Pin<Box<StreamingCompletionResponse<R>>>,
    pending: Vec<CxStreamEvent>,
    finalized: bool,
}

fn map_stream<R>(
    stream: Pin<Box<StreamingCompletionResponse<R>>>,
) -> BoxStream<'static, Result<CxStreamEvent>>
where
    R: Clone + Unpin + GetTokenUsage + Send + 'static,
{
    let state = AdapterStreamState {
        stream,
        pending: Vec::new(),
        finalized: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        if let Some(ev) = state.pending.pop() {
            return Some((Ok(ev), state));
        }
        if state.finalized {
            return None;
        }
        loop {
            match state.stream.next().await {
                None => {
                    state.finalized = true;
                    let mut tail: Vec<CxStreamEvent> = Vec::new();
                    if let Some(u) = state.stream.response.as_ref().and_then(|r| r.token_usage()) {
                        tail.push(CxStreamEvent::Usage {
                            input: u.input_tokens,
                            output: u.output_tokens,
                            cache_read: u.cached_input_tokens,
                            cache_write: u.cache_creation_input_tokens,
                            reasoning: u.reasoning_tokens,
                        });
                    }
                    tail.push(CxStreamEvent::Done);
                    // pending 是 Vec.pop()，所以反向 push
                    state.pending = tail.into_iter().rev().collect();
                    return state.pending.pop().map(|ev| (Ok(ev), state));
                }
                Some(Err(e)) => {
                    state.finalized = true;
                    return Some((Ok(CxStreamEvent::Error(format!("{e}"))), state));
                }
                Some(Ok(item)) => {
                    let events = streamed_to_cx(item);
                    if events.is_empty() {
                        continue;
                    }
                    let (head, rest) = events.split_first().unwrap();
                    let head = head.clone();
                    if !rest.is_empty() {
                        state.pending = rest.iter().rev().cloned().collect();
                    }
                    return Some((Ok(head), state));
                }
            }
        }
    }))
}

fn streamed_to_cx<R>(item: StreamedAssistantContent<R>) -> Vec<CxStreamEvent>
where
    R: Clone + Unpin,
{
    match item {
        StreamedAssistantContent::Text(t) => vec![CxStreamEvent::TextDelta(t.text)],
        StreamedAssistantContent::Reasoning(r) => {
            let text = reasoning_block_text(&r);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![CxStreamEvent::ReasoningDelta(text)]
            }
        }
        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
            if reasoning.is_empty() {
                Vec::new()
            } else {
                vec![CxStreamEvent::ReasoningDelta(reasoning)]
            }
        }
        StreamedAssistantContent::ToolCall { tool_call, .. } => {
            let id = if !tool_call.id.is_empty() {
                tool_call.id.clone()
            } else {
                tool_call.call_id.clone().unwrap_or_default()
            };
            vec![
                CxStreamEvent::ToolCallStart {
                    id: id.clone(),
                    name: tool_call.function.name.clone(),
                },
                CxStreamEvent::ToolCallDone {
                    id,
                    name: tool_call.function.name.clone(),
                    arguments: tool_call.function.arguments.clone(),
                },
            ]
        }
        StreamedAssistantContent::ToolCallDelta { id, content, .. } => match content {
            ToolCallDeltaContent::Name(name) => vec![CxStreamEvent::ToolCallStart { id, name }],
            ToolCallDeltaContent::Delta(partial) => {
                vec![CxStreamEvent::ToolCallArgsDelta { id, partial }]
            }
        },
        StreamedAssistantContent::Final(_) => Vec::new(),
    }
}

fn reasoning_block_text(r: &rig_core::message::Reasoning) -> String {
    let mut out = String::new();
    for c in &r.content {
        if let RigReasoningContent::Text { text, .. } = c {
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{build_rig_history, streamed_to_cx};
    use crate::cx_agent::events::CxStreamEvent;
    use crate::cx_agent::history::{CxContent, CxMessage};
    use crate::cx_agent::provider::{CxToolDefinition, CxTurnRequest};
    use rig_core::message::{Message as RigMessage, ToolCall as RigToolCall, ToolFunction};
    use rig_core::streaming::{StreamedAssistantContent, ToolCallDeltaContent};

    #[test]
    fn build_rig_history_keeps_tool_result_as_prompt() {
        let request = CxTurnRequest {
            system: Some("system".to_string()),
            history: vec![
                CxMessage::user_text("hello"),
                CxMessage::ToolResult {
                    call_id: "call-1".to_string(),
                    name: Some("read_file".to_string()),
                    content: "ok".to_string(),
                    is_error: false,
                },
            ],
            tools: Vec::<CxToolDefinition>::new(),
            model_id: "demo".to_string(),
            max_tokens: Some(32),
        };

        let (history, prompt) = build_rig_history(&request, true).expect("history should build");
        assert_eq!(history.len(), 1);
        assert!(matches!(history[0], RigMessage::User { .. }));
        assert!(matches!(prompt, RigMessage::User { .. }));
    }

    #[test]
    fn streamed_to_cx_maps_tool_call_events() {
        let tool_call = RigToolCall {
            id: "tool-1".to_string(),
            call_id: Some("call-1".to_string()),
            function: ToolFunction {
                name: "write_file".to_string(),
                arguments: json!({"path":"hello.txt","content":"hi"}),
            },
            signature: None,
            additional_params: None,
        };

        let events = streamed_to_cx::<()>(StreamedAssistantContent::ToolCall {
            tool_call,
            internal_call_id: String::new(),
        });
        assert!(matches!(
            &events[0],
            CxStreamEvent::ToolCallStart { id, name }
            if id == "tool-1" && name == "write_file"
        ));
        assert!(matches!(
            &events[1],
            CxStreamEvent::ToolCallDone { id, name, .. }
            if id == "tool-1" && name == "write_file"
        ));

        let delta = streamed_to_cx::<()>(StreamedAssistantContent::ToolCallDelta {
            id: "tool-2".to_string(),
            content: ToolCallDeltaContent::Delta("{".to_string()),
            internal_call_id: String::new(),
        });
        assert!(matches!(
            &delta[0],
            CxStreamEvent::ToolCallArgsDelta { id, partial }
            if id == "tool-2" && partial == "{"
        ));
    }

    #[test]
    fn streamed_to_cx_drops_empty_reasoning() {
        let events = streamed_to_cx::<()>(StreamedAssistantContent::ReasoningDelta {
            id: None,
            reasoning: String::new(),
        });
        assert!(events.is_empty());
    }

    #[test]
    fn assistant_history_preserves_text_reasoning_and_tool_calls() {
        let request = CxTurnRequest {
            system: None,
            history: vec![CxMessage::Assistant {
                content: vec![
                    CxContent::Reasoning {
                        text: "thinking".to_string(),
                    },
                    CxContent::Text {
                        text: "answer".to_string(),
                    },
                    CxContent::ToolCall {
                        id: "tool-9".to_string(),
                        call_id: Some("call-9".to_string()),
                        name: "grep".to_string(),
                        arguments: json!({"pattern":"foo"}),
                    },
                ],
            }],
            tools: Vec::<CxToolDefinition>::new(),
            model_id: "demo".to_string(),
            max_tokens: None,
        };

        let (_, prompt) = build_rig_history(&request, true).expect("history should build");
        assert!(matches!(prompt, RigMessage::Assistant { .. }));
    }

    #[test]
    fn build_rig_history_falls_back_to_plain_text_tool_results_for_openai_like_apis() {
        let request = CxTurnRequest {
            system: None,
            history: vec![CxMessage::ToolResult {
                call_id: "call-2".to_string(),
                name: Some("read_file".to_string()),
                content: "package = cx".to_string(),
                is_error: false,
            }],
            tools: Vec::<CxToolDefinition>::new(),
            model_id: "demo".to_string(),
            max_tokens: None,
        };

        let (_, prompt) = build_rig_history(&request, false).expect("history should build");
        match prompt {
            RigMessage::User { content } => {
                let rendered = format!("{content:?}");
                assert!(rendered.contains("read_file"));
                assert!(rendered.contains("package = cx"));
            }
            other => panic!("expected user prompt, got {other:?}"),
        }
    }
}
