//! `LanguageModel` implementation for the OpenAI Responses wire.
//!
//! - Request: POST `{endpoint}/responses`, `Authorization: Bearer`, body
//!   `{model, input, max_output_tokens, stream:true}`.
//! - Response: SSE event stream; key event types:
//!   - `response.output_text.delta` → text delta
//!   - `response.completed` → done
//!   - `response.failed` / `error` → error
//!
//! Text-only streaming (no reasoning/tool mapping; same policy as the Completions wire).

use anyhow::{Context as _, Result, anyhow};
use futures::{StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::AsyncApp;
use serde::Deserialize;

use crate::language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    MessageContent, Role, StopReason,
};
use crate::provider::sse::extract_data_line;

pub struct ResponsesModel {
    id: String,
    name: String,
    provider_name: String,
    api_model_id: String,
    endpoint_url: String,
    api_key: String,
    max_output_tokens: u64,
    max_token_count: u64,
}

impl ResponsesModel {
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

impl LanguageModel for ResponsesModel {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn provider_id(&self) -> String {
        format!("responses:{}", self.provider_name)
    }
    fn provider_name(&self) -> String {
        self.provider_name.clone()
    }
    fn max_token_count(&self) -> u64 {
        self.max_token_count
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        _cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<LanguageModelCompletionEvent>>>> {
        let url = responses_url(&self.endpoint_url);
        let api_key = self.api_key.clone();
        let model = self.api_model_id.clone();
        let max_tokens = self.max_output_tokens;

        Box::pin(async move {
            let (tx, rx) = async_channel::bounded::<Result<LanguageModelCompletionEvent>>(64);
            let tx_clone = tx.clone();
            crate::runtime::handle().spawn(async move {
                if let Err(e) =
                    stream_responses(&url, &api_key, &model, max_tokens, request, tx_clone).await
                {
                    let _ = tx.send(Err(e)).await;
                }
            });
            let stream: BoxStream<'static, Result<LanguageModelCompletionEvent>> = Box::pin(rx);
            Ok(stream)
        })
    }
}

fn responses_url(endpoint: &str) -> String {
    if endpoint.ends_with("/responses") {
        endpoint.to_string()
    } else {
        format!("{}/responses", endpoint.trim_end_matches('/'))
    }
}

pub async fn stream_responses(
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
        .context("调用 Responses API 失败")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("Responses API 返回 {status}: {body}"));
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
    let input = build_input(&request.messages);
    json!({
        "model": model,
        "input": input,
        "max_output_tokens": max_tokens,
        "stream": true,
    })
}

/// Responses-wire input: a message array with role user/system/assistant and
/// string content (text parts concatenated; non-text folded into a placeholder).
fn build_input(messages: &[LanguageModelRequestMessage]) -> Vec<serde_json::Value> {
    use serde_json::json;
    messages
        .iter()
        .filter_map(|m| {
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
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct ResponsesEvent {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    response: Option<serde_json::Value>,
}

fn map_event(value: &serde_json::Value) -> Vec<Result<LanguageModelCompletionEvent>> {
    let Ok(ev) = serde_json::from_value::<ResponsesEvent>(value.clone()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match ev.ty.as_str() {
        "response.output_text.delta" => {
            if let Some(d) = ev.delta
                && !d.is_empty()
            {
                out.push(Ok(LanguageModelCompletionEvent::Text(d)));
            }
        }
        "response.completed" => {
            out.push(Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn)));
        }
        "response.incomplete" => {
            out.push(Ok(LanguageModelCompletionEvent::Stop(StopReason::MaxTokens)));
        }
        "response.failed" | "error" => {
            let msg = ev
                .response
                .and_then(|r| r.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).map(String::from))
                .unwrap_or_else(|| "Responses API 返回错误".to_string());
            out.push(Err(anyhow!(msg)));
        }
        _ => {}
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

    /// Live streaming test: send "hi" via the Bailian qwen3.7-plus responses wire.
    #[tokio::test]
    async fn live_responses_stream() {
        if std::env::var("MANOX_RUN_LIVE").is_err() {
            return;
        }
        let config = crate::provider::config::CxConfig::load_default().expect("load config");
        let model = config
            .resolve_all_models()
            .into_iter()
            .find(|m| {
                m.provider_name == "百炼" && m.id.contains("qwen3.7-plus") && m.wire_api == WireApi::Responses
            })
            .expect("应含百炼 qwen3.7-plus responses");
        let api_key = crate::provider::resolve_apikey(
            model.apikey_source.as_deref().unwrap_or("env:DASHSCOPE_API_KEY"),
        )
        .expect("resolve api key");

        let (tx, rx) = async_channel::bounded(64);
        let tx_clone = tx.clone();
        let url = responses_url(&model.endpoint_url);
        let api_model = model.api_model_id();
        tokio::spawn(async move {
            if let Err(e) =
                stream_responses(&url, &api_key, &api_model, 64, simple_request("hi"), tx_clone).await
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
