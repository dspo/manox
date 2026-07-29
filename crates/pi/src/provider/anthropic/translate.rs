// Translation between pi's domain types and the Anthropic wire types.
//
// The domain types in `crate::types` already mirror the protocol's block
// shapes, so this layer is thin. The two non-trivial jobs are:
//   1. Restructuring messages — the API requires every `tool_result` to live
//      inside a `role: "user"` message, so consecutive domain `ToolResult`
//      messages are merged into one user message.
//   2. Supplying defaults the API requires — `max_tokens`, thinking budgets.

use crate::types::{
    AgentContext, AgentMessage, ContentBlock, ImageSource, StreamOptions,
};
use super::wire::*;

/// Fallback max_tokens when the caller doesn't specify one.
const DEFAULT_MAX_TOKENS: usize = 8192;

/// Map a thinking level string to a token budget.
///
/// The API takes an explicit `budget_tokens`; pi reasons in named levels. This
/// budget-based mapping targets older models. Adaptive-thinking models use an
/// effort field instead — see the plan's open questions.
fn thinking_budget(level: &str) -> Option<usize> {
    match level {
        "low" => Some(1_024),
        "medium" => Some(8_192),
        "high" => Some(32_768),
        _ => None,
    }
}

/// Build the API request body from the agent context and stream options.
pub fn to_request(context: &AgentContext, options: &StreamOptions) -> MessageCreateParams {
    MessageCreateParams {
        model: context.model.id.clone(),
        max_tokens: options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system: Some(vec![SystemBlock {
            kind: "text",
            text: context.system_prompt.clone(),
            cache_control: cache_control(options),
        }]),
        messages: to_message_params(&context.messages),
        tools: if context.tools.is_empty() {
            None
        } else {
            Some(
                context
                    .tools
                    .iter()
                    .map(|t| ToolParam {
                        name: t.name().to_string(),
                        description: Some(t.description().to_string()),
                        input_schema: t.parameters_schema(),
                        cache_control: None,
                    })
                    .collect(),
            )
        },
        thinking: thinking_config(context),
        temperature: options.temperature,
        stop_sequences: None,
        stream: true,
    }
}

fn cache_control(options: &StreamOptions) -> Option<CacheControl> {
    options.cache_retention.as_ref()?;
    let ttl = match options.cache_retention.as_deref() {
        Some("1h") => Some("1h"),
        _ => None, // "5m" and anything unrecognised falls back to the default 5m
    };
    Some(CacheControl { kind: "ephemeral", ttl })
}

fn thinking_config(context: &AgentContext) -> Option<ThinkingConfig> {
    if !context.model.supports_thinking {
        return None;
    }
    let level = context.thinking_level.as_deref()?;
    thinking_budget(level).map(|budget_tokens| ThinkingConfig::Enabled { budget_tokens })
}

/// Convert domain messages to wire messages, merging every run of consecutive
/// `ToolResult` messages into a single user message of `tool_result` blocks.
fn to_message_params(messages: &[AgentMessage]) -> Vec<MessageParam> {
    let mut out: Vec<MessageParam> = Vec::new();
    let mut pending_results: Vec<ContentBlockParam> = Vec::new();

    // Flush accumulated tool_results as one user message.
    fn flush(out: &mut Vec<MessageParam>, pending: &mut Vec<ContentBlockParam>) {
        if !pending.is_empty() {
            out.push(MessageParam {
                role: Role::User,
                content: std::mem::take(pending),
            });
        }
    }

    for msg in messages {
        match msg {
            AgentMessage::ToolResult { tool_call_id, content, is_error, .. } => {
                pending_results.push(ContentBlockParam::ToolResult {
                    tool_use_id: tool_call_id.clone(),
                    content: content.iter().map(block_to_param).collect(),
                    is_error: if *is_error { Some(true) } else { None },
                });
            }
            AgentMessage::User { content, .. } => {
                flush(&mut out, &mut pending_results);
                out.push(MessageParam {
                    role: Role::User,
                    content: content.iter().map(block_to_param).collect(),
                });
            }
            AgentMessage::Assistant { content, .. } => {
                flush(&mut out, &mut pending_results);
                out.push(MessageParam {
                    role: Role::Assistant,
                    content: content.iter().filter_map(assistant_block_to_param).collect(),
                });
            }
            AgentMessage::Custom { .. } => {
                // Custom messages are harness-internal; never sent to the API.
            }
        }
    }
    flush(&mut out, &mut pending_results);
    out
}

/// Map a content block that appears in a user/tool_result message.
fn block_to_param(block: &ContentBlock) -> ContentBlockParam {
    match block {
        ContentBlock::Text { text } => ContentBlockParam::Text {
            text: text.clone(),
            cache_control: None,
        },
        ContentBlock::Image { source } => ContentBlockParam::Image {
            source: image_source(source),
        },
        // tool_use / thinking don't appear in user messages; degrade to text.
        ContentBlock::ToolUse { name, .. } => ContentBlockParam::Text {
            text: format!("[tool_use: {name}]"),
            cache_control: None,
        },
        ContentBlock::Thinking { thinking, .. } => ContentBlockParam::Text {
            text: thinking.clone(),
            cache_control: None,
        },
        ContentBlock::RedactedThinking { .. } => ContentBlockParam::Text {
            text: String::new(),
            cache_control: None,
        },
    }
}

/// Map an assistant content block. Thinking blocks round-trip with their
/// signature intact — required for multi-turn thinking continuity. A thinking
/// block without a signature is dropped, since the API rejects signatureless
/// thinking params.
fn assistant_block_to_param(block: &ContentBlock) -> Option<ContentBlockParam> {
    match block {
        ContentBlock::Text { text } => Some(ContentBlockParam::Text {
            text: text.clone(),
            cache_control: None,
        }),
        ContentBlock::ToolUse { id, name, input } => Some(ContentBlockParam::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        }),
        ContentBlock::Thinking { thinking, signature } => signature
            .as_ref()
            .map(|sig| ContentBlockParam::Thinking {
                thinking: thinking.clone(),
                signature: sig.clone(),
            }),
        ContentBlock::RedactedThinking { data } => {
            Some(ContentBlockParam::RedactedThinking { data: data.clone() })
        }
        ContentBlock::Image { source } => Some(ContentBlockParam::Image {
            source: image_source(source),
        }),
    }
}

fn image_source(source: &ImageSource) -> ImageSourceParam {
    match source {
        ImageSource::Base64 { media_type, data } => ImageSourceParam::Base64 {
            media_type: media_type.clone(),
            data: data.clone(),
        },
        ImageSource::Url { url } => ImageSourceParam::Url { url: url.clone() },
    }
}

/// Parse a protocol stop_reason string into the domain enum.
pub fn parse_stop_reason(s: &str) -> Option<crate::types::StopReason> {
    use crate::types::StopReason::*;
    Some(match s {
        "end_turn" => EndTurn,
        "max_tokens" => MaxTokens,
        "stop_sequence" => StopSequence,
        "tool_use" => ToolUse,
        "pause_turn" => PauseTurn,
        "refusal" => Refusal,
        "model_context_window_exceeded" => ModelContextWindowExceeded,
        _ => return None,
    })
}

/// Fold two usage reports (initial + delta) into the domain `Usage`.
pub fn to_usage(wire: &WireUsage) -> crate::types::Usage {
    crate::types::Usage {
        input_tokens: wire.input_tokens,
        output_tokens: wire.output_tokens,
        cache_read_input_tokens: wire.cache_read_input_tokens,
        cache_creation_input_tokens: wire.cache_creation_input_tokens,
        cache_creation: wire.cache_creation.as_ref().map(|c| crate::types::CacheCreation {
            ephemeral_1h_input_tokens: c.ephemeral_1h_input_tokens,
            ephemeral_5m_input_tokens: c.ephemeral_5m_input_tokens,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentMessage, ContentBlock, Model, StopReason, Usage};
    use serde_json::json;

    fn model(supports_thinking: bool) -> Model {
        Model {
            provider: "anthropic".into(),
            id: "claude-test".into(),
            context_window: 200_000,
            supports_thinking,
            metadata: Default::default(),
        }
    }

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(text)
    }

    fn tool_result(id: &str, text: &str) -> AgentMessage {
        AgentMessage::ToolResult {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
            is_error: false,
            details: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn assistant_tool_call(id: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "read".into(),
                input: json!({"path": "x"}),
            }],
            model: "claude-test".into(),
            provider: "anthropic".into(),
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage::default(),
            timestamp: chrono::Utc::now(),
        }
    }

    fn ctx(messages: Vec<AgentMessage>, thinking: Option<&str>) -> AgentContext {
        AgentContext {
            system_prompt: "sys".into(),
            messages,
            tools: Vec::new(),
            model: model(thinking.is_some()),
            thinking_level: thinking.map(|s| s.into()),
            metadata: Default::default(),
        }
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let ctx = ctx(
            vec![
                user("read two files"),
                assistant_tool_call("t1"),
                tool_result("t1", "aaa"),
                tool_result("t2", "bbb"),
                user("next"),
            ],
            None,
        );
        let req = to_request(&ctx, &StreamOptions::default());
        // user, assistant, merged-user(tool_result x2), user
        assert_eq!(req.messages.len(), 4);
        let merged = &req.messages[2];
        assert!(matches!(merged.role, Role::User));
        assert_eq!(merged.content.len(), 2);
        assert!(merged
            .content
            .iter()
            .all(|b| matches!(b, ContentBlockParam::ToolResult { .. })));
    }

    #[test]
    fn assistant_tool_use_maps_input_field() {
        let ctx = ctx(vec![assistant_tool_call("t1")], None);
        let req = to_request(&ctx, &StreamOptions::default());
        let v = serde_json::to_value(&req.messages[0]).unwrap();
        assert_eq!(v["content"][0]["type"], "tool_use");
        assert_eq!(v["content"][0]["input"], json!({"path": "x"}));
        assert!(v["content"][0].get("arguments").is_none());
    }

    #[test]
    fn thinking_block_roundtrips_with_signature() {
        let msg = AgentMessage::Assistant {
            content: vec![ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: Some("sig123".into()),
            }],
            model: "m".into(),
            provider: "anthropic".into(),
            stop_reason: None,
            usage: Usage::default(),
            timestamp: chrono::Utc::now(),
        };
        let ctx = ctx(vec![msg], None);
        let req = to_request(&ctx, &StreamOptions::default());
        let v = serde_json::to_value(&req.messages[0]).unwrap();
        assert_eq!(v["content"][0]["type"], "thinking");
        assert_eq!(v["content"][0]["signature"], "sig123");
    }

    #[test]
    fn thinking_config_injected_only_for_thinking_models() {
        let with = ctx(vec![user("hi")], Some("high"));
        assert!(matches!(
            to_request(&with, &StreamOptions::default()).thinking,
            Some(ThinkingConfig::Enabled { budget_tokens: 32_768 })
        ));

        let mut no_think = ctx(vec![user("hi")], None);
        no_think.model = model(false);
        no_think.thinking_level = Some("high".into());
        assert!(to_request(&no_think, &StreamOptions::default()).thinking.is_none());
    }

    #[test]
    fn cache_control_on_system_and_default_max_tokens() {
        let ctx = ctx(vec![user("hi")], None);
        let opts = StreamOptions {
            cache_retention: Some("1h".into()),
            ..Default::default()
        };
        let req = to_request(&ctx, &opts);
        assert_eq!(req.max_tokens, DEFAULT_MAX_TOKENS);
        let v = serde_json::to_value(&req.system).unwrap();
        assert_eq!(v[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(v[0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn stop_reason_parses_all_protocol_values() {
        use crate::types::StopReason::*;
        for (s, want) in [
            ("end_turn", EndTurn),
            ("max_tokens", MaxTokens),
            ("stop_sequence", StopSequence),
            ("tool_use", ToolUse),
            ("pause_turn", PauseTurn),
            ("refusal", Refusal),
            ("model_context_window_exceeded", ModelContextWindowExceeded),
        ] {
            assert_eq!(parse_stop_reason(s), Some(want));
        }
        assert_eq!(parse_stop_reason("nonsense"), None);
    }
}
