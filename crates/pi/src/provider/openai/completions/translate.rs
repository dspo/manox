// Translation between pi's domain types and the Chat Completions wire types.
//
// The protocol keeps tool calls inside the assistant message (`tool_calls`)
// and gives every tool result its own `role: "tool"` message, so conversion
// is mostly 1:1. The opinionated parts:
//   - assistant content is always a plain string, never parts;
//   - prior thinking is dropped from history — the protocol carries no
//     reasoning in context, and re-sending it would bill tokens the model
//     never re-reads;
//   - a tool result's error bit has no wire field and folds into the content
//     as an `[error] ` prefix;
//   - tool-result images follow their run of tool messages in a separate
//     user message, the only position that accepts image parts.

use super::wire::*;
use crate::provider::openai::{
    clamp_cache_key, requires_reasoning_content_on_assistant, uses_legacy_max_tokens,
};
use crate::types::{
    AgentContext, AgentMessage, CacheRetention, ContentBlock, StreamOptions, ThinkingKind,
};

/// Build the API request body from the agent context and stream options.
pub fn to_request(
    context: &AgentContext,
    options: &StreamOptions,
    base_url: &str,
) -> ChatCompletionParams {
    let (max_tokens, max_completion_tokens) = match options.max_tokens {
        Some(n) if uses_legacy_max_tokens(&context.model.provider, base_url) => (Some(n), None),
        Some(n) => (None, Some(n)),
        None => (None, None),
    };
    let (thinking, reasoning_effort) = thinking_params(context);
    let messages = crate::provider::transform::repair_tool_flow(&context.messages);
    ChatCompletionParams {
        model: context.model.id.clone(),
        messages: to_message_params(context, &messages, base_url),
        stream: true,
        max_tokens,
        max_completion_tokens,
        temperature: options.temperature,
        tools: tools_param(context),
        thinking,
        reasoning_effort,
        prompt_cache_key: prompt_cache_key(context, base_url),
        prompt_cache_retention: match context.cache_retention {
            CacheRetention::Long => Some("24h"),
            _ => None,
        },
        stream_options: WireStreamOptions {
            include_usage: true,
        },
    }
}

/// The session-affinity cache key goes to the official OpenAI endpoint
/// whenever caching is on, and to any endpoint under extended retention.
fn prompt_cache_key(context: &AgentContext, base_url: &str) -> Option<String> {
    let send = (base_url.contains("api.openai.com")
        && context.cache_retention != CacheRetention::None)
        || context.cache_retention == CacheRetention::Long;
    if send {
        context.session_id.as_deref().map(clamp_cache_key)
    } else {
        None
    }
}

/// The thinking fields mirror the model's declared mechanism. An `Enabled`
/// model takes the on/off switch, with the level passed alongside as
/// `reasoning_effort` — endpoints that speak the switch also accept the
/// dial. An `Adaptive` model takes the dial alone. `"off"` forces the
/// explicit off state; no level omits both fields (server default); levels
/// pass through unclamped.
fn thinking_params(context: &AgentContext) -> (Option<ThinkingParam>, Option<String>) {
    let level = context.thinking_level.as_deref();
    match context.model.thinking {
        ThinkingKind::None => (None, None),
        ThinkingKind::Enabled => match level {
            None => (None, None),
            Some("off") => (Some(ThinkingParam::Disabled), None),
            Some(level) => (Some(ThinkingParam::Enabled), Some(level.to_string())),
        },
        ThinkingKind::Adaptive => match level {
            None => (None, None),
            Some("off") => (None, Some("none".to_string())),
            Some(level) => (None, Some(level.to_string())),
        },
    }
}

fn tools_param(context: &AgentContext) -> Option<Vec<ToolParam>> {
    if context.tools.is_empty() {
        return None;
    }
    Some(
        context
            .tools
            .iter()
            .map(|t| ToolParam {
                kind: "function",
                function: FunctionParam {
                    name: t.name().to_string(),
                    description: Some(t.description().to_string()),
                    parameters: t.parameters_schema(),
                },
            })
            .collect(),
    )
}

fn to_message_params(
    context: &AgentContext,
    messages: &[AgentMessage],
    base_url: &str,
) -> Vec<MessageParam> {
    let mut out: Vec<MessageParam> = Vec::new();
    if !context.system_prompt.is_empty() {
        out.push(MessageParam::System {
            content: context.system_prompt.clone(),
        });
    }
    let reasoning_backfill = context.model.supports_thinking()
        && requires_reasoning_content_on_assistant(&context.model.provider, base_url);
    // Images from a run of consecutive tool results, flushed as one user
    // message after the run.
    let mut pending_images: Vec<UserPart> = Vec::new();

    for msg in messages {
        match msg {
            AgentMessage::User { content, .. } => {
                flush_images(&mut out, &mut pending_images);
                if let Some(content) = user_content(content) {
                    out.push(MessageParam::User { content });
                }
            }
            AgentMessage::Assistant { content, .. } => {
                flush_images(&mut out, &mut pending_images);
                if let Some(param) = assistant_param(content, reasoning_backfill) {
                    out.push(param);
                }
            }
            AgentMessage::ToolResult {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                let (text, images) = tool_result_parts(content);
                let text = if *is_error {
                    format!("[error] {text}")
                } else {
                    text
                };
                out.push(MessageParam::Tool {
                    tool_call_id: tool_call_id.clone(),
                    content: text,
                });
                pending_images.extend(images);
            }
            // Custom messages are harness-internal; never sent to the API.
            AgentMessage::Custom { .. } => {}
        }
    }
    flush_images(&mut out, &mut pending_images);
    out
}

fn flush_images(out: &mut Vec<MessageParam>, pending: &mut Vec<UserPart>) {
    if !pending.is_empty() {
        out.push(MessageParam::User {
            content: UserContent::Parts(std::mem::take(pending)),
        });
    }
}

/// User content is a plain string unless images force the parts encoding.
fn user_content(blocks: &[ContentBlock]) -> Option<UserContent> {
    let mut text = String::new();
    let mut parts: Vec<UserPart> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text {
                text: t,
                signature: None,
            } => {
                text.push_str(t);
                parts.push(UserPart::Text { text: t.clone() });
            }
            ContentBlock::Image { data, mime_type } => parts.push(image_part(data, mime_type)),
            // Tool calls/results and thinking don't appear in user messages.
            _ => {}
        }
    }
    if parts.iter().any(|p| matches!(p, UserPart::ImageUrl { .. })) {
        Some(UserContent::Parts(parts))
    } else if !text.is_empty() {
        Some(UserContent::Text(text))
    } else {
        None
    }
}

/// An assistant turn reduces to its final text, its tool calls, and — for
/// endpoints that require the field — an empty `reasoning_content`. A turn
/// with neither text nor tool calls carries no information and is skipped.
fn assistant_param(blocks: &[ContentBlock], reasoning_backfill: bool) -> Option<MessageParam> {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text {
                text: t,
                signature: None,
            } => text.push_str(t),
            ContentBlock::ToolUse {
                id, name, input, ..
            } => tool_calls.push(ToolCallParam {
                id: id.clone(),
                kind: "function",
                function: ToolCallFunctionParam {
                    name: name.clone(),
                    arguments: input.to_string(),
                },
            }),
            // Thinking, redacted thinking, and images have no request-side
            // representation.
            _ => {}
        }
    }
    let content = if text.is_empty() { None } else { Some(text) };
    let tool_calls = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };
    if content.is_none() && tool_calls.is_none() {
        return None;
    }
    Some(MessageParam::Assistant {
        content,
        tool_calls,
        reasoning_content: reasoning_backfill.then(String::new),
    })
}

/// A tool result splits into its text (the only content a `tool` message
/// carries) and any images (forwarded separately). Placeholders keep the
/// message non-empty, which some endpoints require.
fn tool_result_parts(blocks: &[ContentBlock]) -> (String, Vec<UserPart>) {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } => texts.push(text.as_str()),
            ContentBlock::Image { data, mime_type } => images.push(image_part(data, mime_type)),
            _ => {}
        }
    }
    let joined = texts.join("\n");
    let text = if !joined.is_empty() {
        joined
    } else if images.is_empty() {
        "(no tool output)".to_string()
    } else {
        "(see attached image)".to_string()
    };
    (text, images)
}

fn image_part(data: &str, mime_type: &str) -> UserPart {
    let url = format!("data:{mime_type};base64,{data}");
    UserPart::ImageUrl {
        image_url: ImageUrlParam { url },
    }
}

/// Map a protocol finish_reason into the domain enum, faithful to the TS Pi
/// map. content_filter/network_error collapse to `Error` (the detail surfaces
/// as `error_message`); unknown reasons also read as `Error` rather than a
/// silent natural stop, so a vendor failure never looks like completion.
pub fn parse_finish_reason(s: &str) -> crate::types::StopReason {
    use crate::types::StopReason::*;
    match s {
        "" | "stop" | "end" => Stop,
        "length" => Length,
        "tool_calls" | "function_call" => ToolUse,
        "content_filter" | "network_error" => Error,
        _ => Error,
    }
}

/// Fold the wire usage into the domain `Usage`.
///
/// The wire's `prompt_tokens` includes the cached subset, so the domain's
/// non-cached `input_tokens` subtracts the hit — otherwise `total_input()`
/// would count the hit twice. Completions-style caching has no creation
/// event, so `cache_creation_input_tokens` stays zero. The wire reports no
/// total; `total_tokens` is the sum of all token classes.
pub fn to_usage(wire: &WireUsage) -> crate::types::Usage {
    let hit = wire
        .prompt_cache_hit_tokens
        .or(wire
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens))
        .unwrap_or(0);
    // `saturating_sub`: a malformed payload with hit > prompt must not
    // underflow into a fabricated huge count.
    let input_tokens = wire.prompt_tokens.unwrap_or(0).saturating_sub(hit);
    let output_tokens = wire.completion_tokens.unwrap_or(0);
    crate::types::Usage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens: hit,
        cache_creation_input_tokens: 0,
        cache_write_1h: None,
        total_tokens: input_tokens + output_tokens + hit,
        reasoning_tokens: wire
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens),
        cost: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentMessage, ContentBlock, Model, Usage};
    use serde_json::json;
    use std::sync::Arc;

    const OPENAI: &str = "https://api.openai.com/v1";

    fn model(thinking: ThinkingKind) -> Model {
        Model {
            provider: "openai".into(),
            id: "gpt-test".into(),
            context_window: 200_000,
            max_tokens: 16_384,
            thinking,
            metadata: Default::default(),
        }
    }

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(text)
    }

    fn assistant(content: Vec<ContentBlock>) -> AgentMessage {
        AgentMessage::Assistant {
            content,
            model: "gpt-test".into(),
            provider: "openai".into(),
            api: "openai_completions".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: None,
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn tool_result(id: &str, content: Vec<ContentBlock>, is_error: bool) -> AgentMessage {
        AgentMessage::ToolResult {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content,
            is_error,
            details: None,
            usage: None,
            added_tool_names: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn ctx(
        messages: Vec<AgentMessage>,
        thinking: ThinkingKind,
        level: Option<&str>,
    ) -> AgentContext {
        AgentContext {
            system_prompt: "sys".into(),
            messages,
            tools: Arc::from(vec![]),
            model: model(thinking),
            thinking_level: level.map(|s| s.into()),
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
        }
    }

    fn request(ctx: &AgentContext) -> serde_json::Value {
        serde_json::to_value(to_request(ctx, &StreamOptions::default(), OPENAI)).unwrap()
    }

    // ── thinking fields ─────────────────────────────────────────────────────

    #[test]
    fn enabled_kind_emits_switch_and_effort() {
        let v = request(&ctx(vec![user("hi")], ThinkingKind::Enabled, Some("high")));
        assert_eq!(v["thinking"], json!({"type": "enabled"}));
        assert_eq!(v["reasoning_effort"], "high");
        // The switch shape carries no budget.
        assert!(v["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn adaptive_kind_emits_effort_only_passthrough() {
        let v = request(&ctx(vec![user("hi")], ThinkingKind::Adaptive, Some("max")));
        assert!(v.get("thinking").is_none());
        // Levels pass through unclamped; the vendor judges them.
        assert_eq!(v["reasoning_effort"], "max");
    }

    #[test]
    fn off_level_forces_explicit_off() {
        let v = request(&ctx(vec![user("hi")], ThinkingKind::Enabled, Some("off")));
        assert_eq!(v["thinking"], json!({"type": "disabled"}));
        assert!(v.get("reasoning_effort").is_none());

        let v = request(&ctx(vec![user("hi")], ThinkingKind::Adaptive, Some("off")));
        assert!(v.get("thinking").is_none());
        assert_eq!(v["reasoning_effort"], "none");
    }

    #[test]
    fn none_level_omits_thinking_fields() {
        for kind in [ThinkingKind::Enabled, ThinkingKind::Adaptive] {
            let v = request(&ctx(vec![user("hi")], kind, None));
            assert!(v.get("thinking").is_none(), "{kind:?}");
            assert!(v.get("reasoning_effort").is_none(), "{kind:?}");
        }
        // A non-thinking model never emits the fields, level or not.
        let v = request(&ctx(vec![user("hi")], ThinkingKind::None, Some("high")));
        assert!(v.get("thinking").is_none());
        assert!(v.get("reasoning_effort").is_none());
    }

    // ── message conversion ──────────────────────────────────────────────────

    #[test]
    fn thinking_blocks_drop_from_history() {
        let msg = assistant(vec![
            ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: Some("sig".into()),

                redacted: None,
            },
            ContentBlock::Text {
                text: "answer".into(),
                signature: None,
            },
        ]);
        let v = request(&ctx(vec![user("q"), msg], ThinkingKind::None, None));
        let m = &v["messages"][2];
        assert_eq!(m["role"], "assistant");
        assert_eq!(m["content"], "answer");
        let serialized = m.to_string();
        assert!(
            !serialized.contains("hmm"),
            "thinking text must not appear: {serialized}"
        );
        assert!(
            !serialized.contains("sig"),
            "signature must not appear: {serialized}"
        );
    }

    #[test]
    fn thinking_only_assistant_turn_is_skipped() {
        let msg = assistant(vec![ContentBlock::Thinking {
            thinking: "hmm".into(),
            signature: None,

            redacted: None,
        }]);
        let v = request(&ctx(
            vec![user("q"), msg, user("again")],
            ThinkingKind::None,
            None,
        ));
        let roles: Vec<&str> = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "user", "user"]);
    }

    #[test]
    fn tool_calls_serialize_with_string_arguments_and_plain_content() {
        let msg = assistant(vec![
            ContentBlock::Text {
                text: "checking".into(),
                signature: None,
            },
            ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: json!({"path": "x"}),
                thought_signature: None,
            },
        ]);
        let v = request(&ctx(vec![msg], ThinkingKind::None, None));
        let m = &v["messages"][1];
        // Content is a plain string, never an array of parts.
        assert_eq!(m["content"], "checking");
        assert_eq!(m["tool_calls"][0]["type"], "function");
        assert_eq!(m["tool_calls"][0]["id"], "t1");
        assert_eq!(m["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(
            m["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"x\"}"
        );
    }

    #[test]
    fn tool_results_get_own_messages_and_error_folds() {
        let text = |s: &str| {
            vec![ContentBlock::Text {
                text: s.into(),
                signature: None,
            }]
        };
        let v = request(&ctx(
            vec![
                user("q"),
                tool_result("t1", text("aaa"), false),
                tool_result("t2", text("boom"), true),
            ],
            ThinkingKind::None,
            None,
        ));
        let msgs = v["messages"].as_array().unwrap();
        // Each result is its own role:tool message — never merged.
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "t1");
        assert_eq!(msgs[2]["content"], "aaa");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "t2");
        // The error bit has no wire field; it folds into the content.
        assert_eq!(msgs[3]["content"], "[error] boom");
    }

    #[test]
    fn orphaned_tool_call_gains_synthetic_error_result() {
        let orphan = assistant(vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "read".into(),
            input: json!({"path": "x"}),
            thought_signature: None,
        }]);
        let v = request(&ctx(vec![user("q"), orphan], ThinkingKind::None, None));
        let msgs = v["messages"].as_array().unwrap();
        // The unpaired call is followed by a synthetic tool message, so the
        // request satisfies the pairing requirement.
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "tool");
        assert_eq!(last["tool_call_id"], "t1");
        assert_eq!(last["content"], "[error] No result provided");
    }

    #[test]
    fn tool_result_images_follow_in_a_user_message() {
        let image = vec![ContentBlock::Image {
            data: "AAAA".into(),
            mime_type: "image/png".into(),
        }];
        let v = request(&ctx(
            vec![user("q"), tool_result("t1", image, false)],
            ThinkingKind::None,
            None,
        ));
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["content"], "(see attached image)");
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(
            msgs[3]["content"][0]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn user_message_with_image_uses_parts_encoding() {
        let msg = AgentMessage::User {
            content: vec![
                ContentBlock::Text {
                    text: "what is this".into(),
                    signature: None,
                },
                ContentBlock::Image {
                    data: "AAAA".into(),
                    mime_type: "image/png".into(),
                },
            ],
            timestamp: chrono::Utc::now(),
        };
        let v = request(&ctx(vec![msg], ThinkingKind::None, None));
        let content = &v["messages"][1]["content"];
        assert_eq!(content[0], json!({"type": "text", "text": "what is this"}));
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    // ── endpoint-conditional fields ─────────────────────────────────────────

    #[test]
    fn max_tokens_field_follows_endpoint() {
        let opts = StreamOptions {
            max_tokens: Some(1024),
            ..Default::default()
        };
        let c = ctx(vec![user("hi")], ThinkingKind::None, None);

        let v = serde_json::to_value(to_request(&c, &opts, OPENAI)).unwrap();
        assert_eq!(v["max_completion_tokens"], 1024);
        assert!(v.get("max_tokens").is_none());

        // Legacy-field endpoints, by URL or by provider id.
        for base in [
            "https://api.moonshot.cn/v1",
            "https://integrate.api.nvidia.com/v1",
        ] {
            let v = serde_json::to_value(to_request(&c, &opts, base)).unwrap();
            assert_eq!(v["max_tokens"], 1024, "{base}");
            assert!(v.get("max_completion_tokens").is_none(), "{base}");
        }

        // Unset limit emits neither field.
        let v = request(&c);
        assert!(v.get("max_tokens").is_none());
        assert!(v.get("max_completion_tokens").is_none());
    }

    #[test]
    fn cache_key_follows_endpoint_and_retention_gate() {
        let mut c = ctx(vec![user("hi")], ThinkingKind::None, None);
        c.session_id = Some("s".repeat(100));

        // Official endpoint + default (short) retention: key sent, clamped to
        // 64 chars; no retention field.
        let v = serde_json::to_value(to_request(&c, &StreamOptions::default(), OPENAI)).unwrap();
        assert_eq!(v["prompt_cache_key"].as_str().unwrap().len(), 64);
        assert!(v.get("prompt_cache_retention").is_none());

        // Non-official endpoint + short retention: neither field.
        let v = serde_json::to_value(to_request(
            &c,
            &StreamOptions::default(),
            "https://example.com/v1",
        ))
        .unwrap();
        assert!(v.get("prompt_cache_key").is_none());
        assert!(v.get("prompt_cache_retention").is_none());

        // Non-official endpoint + long retention: key and 24h retention.
        c.cache_retention = crate::types::CacheRetention::Long;
        let v = serde_json::to_value(to_request(
            &c,
            &StreamOptions::default(),
            "https://example.com/v1",
        ))
        .unwrap();
        assert_eq!(v["prompt_cache_key"].as_str().unwrap().len(), 64);
        assert_eq!(v["prompt_cache_retention"], "24h");

        // Retention off: neither field, even on the official endpoint.
        c.cache_retention = crate::types::CacheRetention::None;
        let v = serde_json::to_value(to_request(&c, &StreamOptions::default(), OPENAI)).unwrap();
        assert!(v.get("prompt_cache_key").is_none());
        assert!(v.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn deepseek_backfills_empty_reasoning_content() {
        let thinking_ctx = || {
            ctx(
                vec![
                    user("q"),
                    assistant(vec![ContentBlock::Text {
                        text: "a".into(),
                        signature: None,
                    }]),
                ],
                ThinkingKind::Enabled,
                Some("high"),
            )
        };

        let v = serde_json::to_value(to_request(
            &thinking_ctx(),
            &StreamOptions::default(),
            "https://api.deepseek.com/v1",
        ))
        .unwrap();
        assert_eq!(v["messages"][2]["reasoning_content"], "");

        // Non-DeepSeek endpoints get no extra field.
        let v = request(&thinking_ctx());
        assert!(v["messages"][2].get("reasoning_content").is_none());

        // A non-thinking model never backfills, even against DeepSeek.
        let plain = ctx(
            vec![
                user("q"),
                assistant(vec![ContentBlock::Text {
                    text: "a".into(),
                    signature: None,
                }]),
            ],
            ThinkingKind::None,
            None,
        );
        let v = serde_json::to_value(to_request(
            &plain,
            &StreamOptions::default(),
            "https://api.deepseek.com/v1",
        ))
        .unwrap();
        assert!(v["messages"][2].get("reasoning_content").is_none());
    }

    // ── usage & finish reason ───────────────────────────────────────────────

    #[test]
    fn usage_subtracts_hit_under_both_spellings() {
        let flat = WireUsage {
            prompt_tokens: Some(1000),
            completion_tokens: Some(50),
            prompt_cache_hit_tokens: Some(800),
            prompt_tokens_details: None,
            completion_tokens_details: None,
        };
        let u = to_usage(&flat);
        assert_eq!(u.input_tokens, 200);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cache_read_input_tokens, 800);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.total_input(), 1000);
        assert_eq!(u.total_tokens, 1050);

        let nested = WireUsage {
            prompt_tokens: Some(1000),
            completion_tokens: Some(50),
            prompt_cache_hit_tokens: None,
            prompt_tokens_details: Some(WirePromptTokensDetails {
                cached_tokens: Some(800),
            }),
            completion_tokens_details: None,
        };
        let u = to_usage(&nested);
        assert_eq!(u.input_tokens, 200);
        assert_eq!(u.cache_read_input_tokens, 800);

        // A malformed hit > prompt saturates instead of underflowing.
        let bad = WireUsage {
            prompt_tokens: Some(10),
            completion_tokens: None,
            prompt_cache_hit_tokens: Some(99),
            prompt_tokens_details: None,
            completion_tokens_details: None,
        };
        assert_eq!(to_usage(&bad).input_tokens, 0);
    }

    #[test]
    fn finish_reason_maps_known_and_unknown() {
        use crate::types::StopReason::*;
        assert_eq!(parse_finish_reason(""), Stop);
        assert_eq!(parse_finish_reason("stop"), Stop);
        assert_eq!(parse_finish_reason("end"), Stop);
        assert_eq!(parse_finish_reason("length"), Length);
        assert_eq!(parse_finish_reason("tool_calls"), ToolUse);
        assert_eq!(parse_finish_reason("function_call"), ToolUse);
        assert_eq!(parse_finish_reason("content_filter"), Error);
        assert_eq!(parse_finish_reason("network_error"), Error);
        // Vendor-invented reasons read as a failure, not a silent completion.
        assert_eq!(parse_finish_reason("vendor_reason"), Error);
    }

    #[test]
    fn system_prompt_leads_and_stream_options_always_on() {
        let v = request(&ctx(vec![user("hi")], ThinkingKind::None, None));
        assert_eq!(
            v["messages"][0],
            json!({"role": "system", "content": "sys"})
        );
        assert_eq!(v["stream_options"]["include_usage"], true);
        assert_eq!(v["stream"], true);
    }
}
