//! `LanguageModel` implementation for the OpenAI Chat Completions wire.
//!
//! - Request: POST `{endpoint}/chat/completions`, `Authorization: Bearer`, body
//!   `{model, messages, max_tokens, stream:true}`.
//! - Response: SSE `data:` lines, each `{"choices":[{"delta":{"content":...},"finish_reason":...}]}`,
//!   terminating with `data: [DONE]`.
//!
//! Text-only streaming (no thinking/tool mapping; the Completions wire is not used for tool turns in manox).

use anyhow::{Context as _, Result, anyhow};
use futures::{StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::AsyncApp;
use serde::Deserialize;

use crate::language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    MessageContent, Role, StopReason,
};
use crate::provider::sse::extract_data_line;

pub struct CompletionsModel {
    id: String,
    name: String,
    provider_name: String,
    api_model_id: String,
    endpoint_url: String,
    api_key: String,
    max_output_tokens: u64,
    max_token_count: u64,
}

impl CompletionsModel {
    pub fn new(
        id: String,
        name: String,
        provider_name: String,
        api_model_id: String,
        endpoint_url: String,
        api_key: String,
        max_token_count: u64,
    ) -> Self {
        Self {
            id,
            name,
            provider_name,
            api_model_id,
            endpoint_url,
            api_key,
            max_output_tokens: max_token_count.min(8192),
            max_token_count,
        }
    }
}

impl LanguageModel for CompletionsModel {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn provider_id(&self) -> String {
        format!("completions:{}", self.provider_name)
    }
    fn provider_name(&self) -> String {
        self.provider_name.clone()
    }
    fn wire_api(&self) -> crate::provider::config::WireApi {
        crate::provider::config::WireApi::Completions
    }
    fn max_token_count(&self) -> u64 {
        self.max_token_count
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        _cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<LanguageModelCompletionEvent>>>> {
        let url = completions_url(&self.endpoint_url);
        let api_key = self.api_key.clone();
        let model = self.api_model_id.clone();
        let max_tokens = self.max_output_tokens;

        Box::pin(async move {
            let (tx, rx) = async_channel::bounded::<Result<LanguageModelCompletionEvent>>(64);
            let tx_clone = tx.clone();
            crate::runtime::handle().spawn(async move {
                if let Err(e) =
                    stream_completions(&url, &api_key, &model, max_tokens, request, tx_clone).await
                {
                    let _ = tx.send(Err(e)).await;
                }
            });
            let stream: BoxStream<'static, Result<LanguageModelCompletionEvent>> = Box::pin(rx);
            Ok(stream)
        })
    }
}

fn completions_url(endpoint: &str) -> String {
    if endpoint.ends_with("/chat/completions") {
        endpoint.to_string()
    } else {
        format!("{}/chat/completions", endpoint.trim_end_matches('/'))
    }
}

pub async fn stream_completions(
    url: &str,
    api_key: &str,
    model: &str,
    max_tokens: u64,
    request: LanguageModelRequest,
    tx: async_channel::Sender<Result<LanguageModelCompletionEvent>>,
) -> Result<()> {
    let body = build_request_body(model, max_tokens, &request);
    let client = reqwest::Client::builder().build().context("构建 reqwest client 失败")?;

    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .context("调用 Completions API 失败")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("Completions API 返回 {status}: {body}"));
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut stopped = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("读取 SSE chunk 失败")?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf = buf[nl + 1..].to_string();
            let Some(data) = extract_data_line(&line) else {
                continue;
            };
            if data == "[DONE]" {
                if !stopped {
                    let _ = tx.send(Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn))).await;
                    stopped = true;
                }
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(Err(anyhow!("解析 SSE 事件失败: {e}"))).await;
                    continue;
                }
            };
            for event in map_event(&value) {
                if matches!(event, Ok(LanguageModelCompletionEvent::Stop(_))) {
                    stopped = true;
                }
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
    if !stopped {
        let _ = tx.send(Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn))).await;
    }
    Ok(())
}

fn build_request_body(
    model: &str,
    max_tokens: u64,
    request: &LanguageModelRequest,
) -> serde_json::Value {
    use serde_json::json;
    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .filter_map(message_to_openai)
        .collect();
    json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
    })
}

/// Map a manox message to an OpenAI chat message (text parts concatenated;
/// Thinking/ToolUse/ToolResult are folded into text or skipped — the Completions
/// wire is used for plain-text conversation only).
fn message_to_openai(m: &LanguageModelRequestMessage) -> Option<serde_json::Value> {
    use serde_json::json;
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    };
    let mut text = String::new();
    for c in &m.content {
        match c {
            MessageContent::Text(t) => text.push_str(t),
            MessageContent::Thinking { text: t, .. } => text.push_str(t),
            MessageContent::ToolResult(tr) => {
                text.push_str(&format!("[tool_result {}]: {}", tr.tool_name, tr.content));
            }
            MessageContent::ToolUse(_) => {}
        }
    }
    if text.trim().is_empty() {
        return None;
    }
    Some(json!({"role": role, "content": text}))
}

#[derive(Debug, Deserialize)]
struct CompletionsChunk {
    #[serde(default)]
    choices: Vec<CompletionsChoice>,
}

#[derive(Debug, Deserialize)]
struct CompletionsChoice {
    #[serde(default)]
    delta: CompletionsDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CompletionsDelta {
    #[serde(default)]
    content: Option<String>,
}

fn map_event(value: &serde_json::Value) -> Vec<Result<LanguageModelCompletionEvent>> {
    let Ok(chunk) = serde_json::from_value::<CompletionsChunk>(value.clone()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content
            && !content.is_empty()
        {
            out.push(Ok(LanguageModelCompletionEvent::Text(content)));
        }
        if let Some(reason) = choice.finish_reason {
            let stop = match reason.as_str() {
                "stop" => StopReason::EndTurn,
                "length" => StopReason::MaxTokens,
                "tool_calls" => StopReason::ToolUse,
                "content_filter" => StopReason::Refusal,
                _ => StopReason::EndTurn,
            };
            out.push(Ok(LanguageModelCompletionEvent::Stop(stop)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::{LanguageModelRequestMessage, MessageContent};
    use crate::provider::config::WireApi;

    fn simple_request(text: &str) -> LanguageModelRequest {
        LanguageModelRequest {
            messages: vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(text.to_string())],
                cache: false,
            }],
            ..Default::default()
        }
    }

    /// Live streaming test: send "hi" via the Bailian qwen3.7-plus completions wire.
    #[tokio::test]
    async fn live_completions_stream() {
        if std::env::var("MANOX_RUN_LIVE").is_err() {
            return;
        }
        let config = crate::provider::config::CxConfig::load_default().expect("load config");
        let model = config
            .resolve_all_models()
            .into_iter()
            .find(|m| {
                m.provider_name == "百炼" && m.id.contains("qwen3.7-plus") && m.wire_api == WireApi::Completions
            })
            .expect("应含百炼 qwen3.7-plus completions");
        let api_key = crate::provider::resolve_apikey(
            model.apikey_source.as_deref().unwrap_or("env:DASHSCOPE_API_KEY"),
        )
        .expect("resolve api key");

        let (tx, rx) = async_channel::bounded(64);
        let tx_clone = tx.clone();
        let url = completions_url(&model.endpoint_url);
        let api_model = model.api_model_id();
        tokio::spawn(async move {
            if let Err(e) =
                stream_completions(&url, &api_key, &api_model, 64, simple_request("hi"), tx_clone).await
            {
                let _ = tx.send(Err(e)).await;
            }
        });

        let mut texts = 0u32;
        let mut stopped = false;
        while let Ok(ev) = rx.recv().await {
            match ev {
                Ok(LanguageModelCompletionEvent::Text(_)) => texts += 1,
                Ok(LanguageModelCompletionEvent::Stop(_)) => {
                    stopped = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => panic!("stream error: {e}"),
            }
        }
        assert!(texts > 0, "应至少收到一个 Text 事件");
        assert!(stopped, "应收到 Stop 事件");
    }
}
