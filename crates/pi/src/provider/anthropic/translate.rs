// Translation between pi's domain types and the Anthropic wire types.
//
// The domain types in `crate::types` already mirror the protocol's block
// shapes, so this layer is thin. The two non-trivial jobs are:
//   1. Restructuring messages — the API requires every `tool_result` to live
//      inside a `role: "user"` message, so consecutive domain `ToolResult`
//      messages are merged into one user message.
//   2. Supplying defaults the API requires — `max_tokens`, thinking budgets.

use crate::types::{
    AgentContext, AgentMessage, CacheRetention, ContentBlock, ImageSource, StreamOptions,
    ThinkingKind,
};
use super::wire::*;

/// Fallback max_tokens when the caller doesn't specify one.
const DEFAULT_MAX_TOKENS: usize = 8192;

/// Map a thinking level to an adaptive-thinking effort.
///
/// pi reasons in named levels; adaptive models take an effort tier instead of
/// a token budget. Unknown levels fall back to `high`, matching the reference
/// mapping. "xhigh"/"max" pass through for the models that support them.
fn map_effort(level: &str) -> Effort {
    match level {
        "minimal" | "low" => Effort::Low,
        "medium" => Effort::Medium,
        "xhigh" => Effort::Xhigh,
        "max" => Effort::Max,
        _ => Effort::High,
    }
}

/// Build the API request body from the agent context and stream options.
pub fn to_request(context: &AgentContext, options: &StreamOptions) -> MessageCreateParams {
    let cache_control = cache_control(context);
    MessageCreateParams {
        model: context.model.id.clone(),
        max_tokens: options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system: Some(vec![SystemBlock {
            kind: "text",
            text: context.system_prompt.clone(),
            cache_control: cache_control.clone(),
        }]),
        messages: to_message_params(&context.messages, cache_control.as_ref()),
        tools: tools_param(context, cache_control.as_ref()),
        thinking: thinking_config(context),
        output_config: output_config(context),
        temperature: options.temperature,
        stop_sequences: None,
        stream: true,
    }
}

/// The cache marker shared by every breakpoint in a request: absent when
/// caching is off, `ttl: "1h"` under extended retention.
fn cache_control(context: &AgentContext) -> Option<CacheControl> {
    match context.cache_retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(CacheControl::ephemeral()),
        CacheRetention::Long => Some(CacheControl { kind: "ephemeral", ttl: Some("1h") }),
    }
}

/// Tool definitions form a cacheable prefix; the breakpoint sits on the last
/// one so the whole list is covered by a single marker.
fn tools_param(context: &AgentContext, cache_control: Option<&CacheControl>) -> Option<Vec<ToolParam>> {
    if context.tools.is_empty() {
        return None;
    }
    let last = context.tools.len() - 1;
    Some(
        context
            .tools
            .iter()
            .enumerate()
            .map(|(i, t)| ToolParam {
                name: t.name().to_string(),
                description: Some(t.description().to_string()),
                input_schema: t.parameters_schema(),
                cache_control: if i == last { cache_control.cloned() } else { None },
            })
            .collect(),
    )
}

/// The thinking field mirrors the model's thinking protocol: `"off"` forces
/// `disabled`, a set level picks `adaptive` or `enabled` per the model's
/// [`ThinkingKind`], and no level at all omits the field (server default).
/// Thinking text comes back summarized; its signature is always preserved for
/// multi-turn continuity.
fn thinking_config(context: &AgentContext) -> Option<ThinkingConfig> {
    let display = Some(ThinkingDisplay::Summarized);
    match context.model.thinking {
        ThinkingKind::None => None,
        kind => match context.thinking_level.as_deref() {
            None => None,
            Some("off") => Some(ThinkingConfig::Disabled),
            Some(_) => Some(match kind {
                ThinkingKind::Adaptive => ThinkingConfig::Adaptive { display },
                ThinkingKind::Enabled => ThinkingConfig::Enabled { display },
                ThinkingKind::None => unreachable!(),
            }),
        },
    }
}

/// The effort tier lives in `output_config`, separate from `thinking`.
fn output_config(context: &AgentContext) -> Option<OutputConfig> {
    if !context.model.supports_thinking() {
        return None;
    }
    let level = context.thinking_level.as_deref()?;
    if level == "off" {
        return None;
    }
    Some(OutputConfig {
        effort: Some(map_effort(level)),
    })
}

/// Convert domain messages to wire messages, merging every run of consecutive
/// `ToolResult` messages into a single user message of `tool_result` blocks.
///
/// The conversation tail is the final cache breakpoint: when caching is on,
/// the last block of the last message carries the marker, provided that
/// message is a user message — a trailing assistant message is never marked.
fn to_message_params(
    messages: &[AgentMessage],
    cache_control: Option<&CacheControl>,
) -> Vec<MessageParam> {
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
                    cache_control: None,
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

    if let Some(cc) = cache_control
        && let Some(last) = out.last_mut()
        && matches!(last.role, Role::User)
        && let Some(block) = last.content.last_mut()
    {
        block.set_cache_control(cc.clone());
    }

    out
}

/// Map a content block that appears in a user/tool_result message.
fn block_to_param(block: &ContentBlock) -> ContentBlockParam {
    match block {
        ContentBlock::Text { text, .. } => ContentBlockParam::Text {
            text: text.clone(),
            cache_control: None,
        },
        ContentBlock::Image { source } => ContentBlockParam::Image {
            source: image_source(source),
            cache_control: None,
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
        ContentBlock::Text { text, .. } => Some(ContentBlockParam::Text {
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
            cache_control: None,
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

    fn model(thinking: ThinkingKind) -> Model {
        Model {
            provider: "anthropic".into(),
            id: "claude-test".into(),
            context_window: 200_000,
            thinking,
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
            content: vec![ContentBlock::Text { text: text.into(), signature: None }],
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

    fn ctx(messages: Vec<AgentMessage>, thinking: ThinkingKind, level: Option<&str>) -> AgentContext {
        AgentContext {
            system_prompt: "sys".into(),
            messages,
            tools: Vec::new(),
            model: model(thinking),
            thinking_level: level.map(|s| s.into()),
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
        }
    }

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl crate::tool::AgentTool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "d"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Value,
            _signal: tokio_util::sync::CancellationToken,
            _ctx: &dyn crate::tool::ToolContext,
        ) -> Result<crate::tool::AgentToolResult, crate::tool::ToolError> {
            unreachable!()
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
            ThinkingKind::None,
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
        let ctx = ctx(vec![assistant_tool_call("t1")], ThinkingKind::None, None);
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
        let ctx = ctx(vec![msg], ThinkingKind::None, None);
        let req = to_request(&ctx, &StreamOptions::default());
        let v = serde_json::to_value(&req.messages[0]).unwrap();
        assert_eq!(v["content"][0]["type"], "thinking");
        assert_eq!(v["content"][0]["signature"], "sig123");
    }

    #[test]
    fn adaptive_thinking_sets_thinking_and_effort() {
        let with = ctx(vec![user("hi")], ThinkingKind::Adaptive, Some("high"));
        let req = to_request(&with, &StreamOptions::default());
        assert!(matches!(
            req.thinking,
            Some(ThinkingConfig::Adaptive { display: Some(ThinkingDisplay::Summarized) })
        ));
        assert!(matches!(
            req.output_config,
            Some(OutputConfig { effort: Some(Effort::High) })
        ));

        // A non-thinking model emits neither, even with a level set.
        let no_think = ctx(vec![user("hi")], ThinkingKind::None, Some("high"));
        let req = to_request(&no_think, &StreamOptions::default());
        assert!(req.thinking.is_none());
        assert!(req.output_config.is_none());
    }

    #[test]
    fn enabled_kind_uses_enabled_shape_without_budget() {
        let ctx = ctx(vec![user("hi")], ThinkingKind::Enabled, Some("high"));
        let req = to_request(&ctx, &StreamOptions::default());
        assert!(matches!(
            req.thinking,
            Some(ThinkingConfig::Enabled { display: Some(ThinkingDisplay::Summarized) })
        ));
        assert!(matches!(
            req.output_config,
            Some(OutputConfig { effort: Some(Effort::High) })
        ));

        // The enabled wire shape carries no budget_tokens — depth comes from effort.
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["thinking"]["type"], "enabled");
        assert!(v["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn off_level_disables_thinking_without_effort() {
        for kind in [ThinkingKind::Enabled, ThinkingKind::Adaptive] {
            let ctx = ctx(vec![user("hi")], kind, Some("off"));
            let req = to_request(&ctx, &StreamOptions::default());
            assert!(matches!(req.thinking, Some(ThinkingConfig::Disabled)), "{kind:?}");
            assert!(req.output_config.is_none(), "{kind:?}");
        }
    }

    #[test]
    fn no_level_omits_thinking_field() {
        for kind in [ThinkingKind::Enabled, ThinkingKind::Adaptive] {
            let ctx = ctx(vec![user("hi")], kind, None);
            let req = to_request(&ctx, &StreamOptions::default());
            assert!(req.thinking.is_none(), "{kind:?}");
            assert!(req.output_config.is_none(), "{kind:?}");
        }
    }

    #[test]
    fn effort_maps_levels_and_passthrough() {
        for (level, want) in [
            ("minimal", Effort::Low),
            ("low", Effort::Low),
            ("medium", Effort::Medium),
            ("high", Effort::High),
            ("xhigh", Effort::Xhigh),
            ("max", Effort::Max),
            ("unknown", Effort::High),
        ] {
            assert!(matches!(map_effort(level), ref e if std::mem::discriminant(e) == std::mem::discriminant(&want)), "level {level}");
        }
    }

    #[test]
    fn thinking_wire_shape_matches_api() {
        let with = ctx(vec![user("hi")], ThinkingKind::Adaptive, Some("max"));
        let req = to_request(&with, &StreamOptions::default());
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["thinking"]["type"], "adaptive");
        assert_eq!(v["thinking"]["display"], "summarized");
        assert_eq!(v["output_config"]["effort"], "max");
        // effort must NOT live inside thinking.
        assert!(v["thinking"].get("effort").is_none());
        assert!(v["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn cache_breakpoints_mark_system_last_tool_and_last_user_message() {
        let mut c = ctx(
            vec![
                user("first"),
                assistant_tool_call("t1"),
                tool_result("t1", "aaa"),
                user("latest"),
            ],
            ThinkingKind::None,
            None,
        );
        c.tools = vec![Box::new(NamedTool("a")), Box::new(NamedTool("b"))];
        let req = to_request(&c, &StreamOptions::default());
        assert_eq!(req.max_tokens, DEFAULT_MAX_TOKENS);

        // Breakpoint 1: the system block.
        let system = serde_json::to_value(&req.system).unwrap();
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert!(system[0]["cache_control"].get("ttl").is_none());

        // Breakpoint 2: the last tool only.
        let tools = serde_json::to_value(&req.tools).unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");

        // Breakpoint 3: the last block of the last (user) message only.
        let messages = serde_json::to_value(&req.messages).unwrap();
        let messages = messages.as_array().unwrap();
        assert!(messages[0]["content"][0].get("cache_control").is_none());
        let last_user = messages.last().unwrap();
        let blocks = last_user["content"].as_array().unwrap();
        assert_eq!(
            blocks.last().unwrap()["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn cache_breakpoint_lands_on_tool_result_block() {
        let c = ctx(
            vec![assistant_tool_call("t1"), tool_result("t1", "aaa")],
            ThinkingKind::None,
            None,
        );
        let req = to_request(&c, &StreamOptions::default());
        let messages = serde_json::to_value(&req.messages).unwrap();
        let messages = messages.as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_retention_none_sends_no_markers() {
        let mut c = ctx(vec![user("hi")], ThinkingKind::None, None);
        c.cache_retention = crate::types::CacheRetention::None;
        c.tools = vec![Box::new(NamedTool("a"))];
        let req = to_request(&c, &StreamOptions::default());
        let v = serde_json::to_value(&req).unwrap();
        assert!(v["system"][0].get("cache_control").is_none());
        assert!(v["tools"][0].get("cache_control").is_none());
        assert!(v["messages"][0]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn cache_retention_long_adds_one_hour_ttl() {
        let mut c = ctx(vec![user("hi")], ThinkingKind::None, None);
        c.cache_retention = crate::types::CacheRetention::Long;
        c.tools = vec![Box::new(NamedTool("a"))];
        let req = to_request(&c, &StreamOptions::default());
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(v["tools"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(v["messages"][0]["content"][0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn trailing_assistant_message_gets_no_breakpoint() {
        let c = ctx(vec![assistant_tool_call("t1")], ThinkingKind::None, None);
        let req = to_request(&c, &StreamOptions::default());
        let messages = serde_json::to_value(&req.messages).unwrap();
        let messages = messages.as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "assistant");
        assert!(last["content"][0].get("cache_control").is_none());
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
