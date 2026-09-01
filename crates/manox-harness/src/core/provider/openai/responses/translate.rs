// Translation between pi's domain types and the Responses wire types.
//
// The protocol is item-oriented: a request replays the full conversation as
// a flat `input` array, and continuity is client-side (`store: false`).
// Conversion therefore centers on faithful replay:
//   - reasoning items round-trip as raw JSON via `Thinking::signature`;
//     without a signature the block is dropped (same model) or flattened to
//     plain text (different model — a foreign reasoning item cannot be
//     replayed, but its content still informs the new model);
//   - assistant text carries a `{v, id, phase}` signature so the replayed
//     message item keeps its server-side identity; positional `msg_pi_*`
//     ids fill in when no signature exists;
//   - tool call ids are `call_id|item_id` pairs; the item id survives
//     replay only when it is a same-model `fc_` id, everything else is
//     normalized away to satisfy the server's call/reasoning pairing
//     validation;
//   - a tool result is output content only — the error bit has no wire
//     representation and is NOT folded into the text on this shape;
//   - tool calls left without a result (edited or truncated history) gain a
//     synthetic error output, because the API rejects unpaired calls.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use super::wire::*;
use crate::core::provider::openai::{clamp_cache_key, ensure_object_properties};
use crate::types::{AgentContext, AgentMessage, CacheRetention, ContentBlock, StreamOptions};

/// Build the API request body from the agent context and stream options.
pub fn to_request(context: &AgentContext, options: &StreamOptions) -> ResponsesParams {
    let (reasoning, include) = reasoning_params(context);
    let messages = crate::core::provider::transform::prepare_for_wire(&context.messages);
    ResponsesParams {
        model: context.model.id.clone(),
        input: to_input(context, &messages),
        stream: true,
        store: false,
        // The API rejects max_output_tokens below 16.
        max_output_tokens: options.max_tokens.map(|n| n.max(16)),
        temperature: options.temperature,
        tools: tools_param(context),
        reasoning,
        include,
        prompt_cache_key: match context.cache_retention {
            CacheRetention::None => None,
            _ => context.session_id.as_deref().map(clamp_cache_key),
        },
        prompt_cache_retention: match context.cache_retention {
            CacheRetention::Long => Some("24h"),
            _ => None,
        },
    }
}

/// A configured thinking level always emits the reasoning object and requests
/// the encrypted payload needed to replay reasoning items under `store: false`.
/// Model metadata does not gate this request. An absent or `"off"` level
/// carries the explicit off state. Levels pass through unclamped.
fn reasoning_params(context: &AgentContext) -> (Option<ReasoningParam>, Option<Vec<&'static str>>) {
    match context.thinking_level.as_deref() {
        Some(level) if level != "off" => (
            Some(ReasoningParam {
                effort: level.to_string(),
                summary: Some("auto"),
            }),
            Some(vec!["reasoning.encrypted_content"]),
        ),
        _ => (
            Some(ReasoningParam {
                effort: "none".to_string(),
                summary: None,
            }),
            None,
        ),
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
                name: t.name().to_string(),
                description: Some(t.description().to_string()),
                parameters: ensure_object_properties(t.parameters_schema()),
            })
            .collect(),
    )
}

/// Convert the conversation into the flat `input` array.
fn to_input(context: &AgentContext, messages: &[AgentMessage]) -> Vec<InputItem> {
    let mut items: Vec<InputItem> = Vec::new();
    // The system prompt leads as a convenient-form message; reasoning models
    // take it under the developer role.
    if !context.system_prompt.is_empty() {
        let role = if context.model.supports_thinking() {
            "developer"
        } else {
            "system"
        };
        items.push(InputItem::Message(InputMessage {
            role,
            content: InputMessageContent::Text(context.system_prompt.clone()),
        }));
    }

    // Positional counter for fallback message ids; only messages that emit
    // items consume an index.
    let mut msg_index = 0usize;
    // Original tool call id -> normalized id, filled while converting
    // cross-model assistant turns so their results can follow.
    let mut id_map: HashMap<String, String> = HashMap::new();

    for msg in messages {
        match msg {
            AgentMessage::User { content, .. } => {
                let parts: Vec<InputPart> = content.iter().filter_map(user_part).collect();
                if parts.is_empty() {
                    continue;
                }
                items.push(InputItem::Message(InputMessage {
                    role: "user",
                    content: InputMessageContent::Parts(parts),
                }));
            }
            AgentMessage::Assistant {
                content,
                model,
                provider,
                ..
            } => {
                let before = items.len();
                convert_assistant(
                    &mut items,
                    content,
                    model,
                    provider,
                    context,
                    msg_index,
                    &mut id_map,
                );
                if items.len() == before {
                    continue;
                }
            }
            AgentMessage::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                let effective = id_map.get(tool_call_id).unwrap_or(tool_call_id).clone();
                let call_id = effective
                    .split('|')
                    .next()
                    .unwrap_or(&effective)
                    .to_string();
                items.push(InputItem::Item(OutputItem::FunctionCallOutput {
                    call_id,
                    output: tool_result_output(content),
                }));
            }
            // `prepare_for_wire` has already projected these onto user
            // messages; the arm only satisfies exhaustiveness.
            AgentMessage::BashExecution { .. } | AgentMessage::Custom { .. } => continue,
        }
        msg_index += 1;
    }
    items
}

/// Convert one assistant turn into its output items. `is_same_model`
/// decides what may be replayed verbatim: only the model that produced an
/// item may see it again — reasoning items, text identities, and `fc_` call
/// ids are all server-side state of a specific model's turn.
fn convert_assistant(
    items: &mut Vec<InputItem>,
    content: &[ContentBlock],
    msg_model: &str,
    msg_provider: &str,
    context: &AgentContext,
    msg_index: usize,
    id_map: &mut HashMap<String, String>,
) {
    let same_model = msg_model == context.model.id && msg_provider == context.model.provider;
    // Same provider, different model: the message's items are
    // well-formed but belong to another model's turn.
    let is_different_model = !same_model && msg_provider == context.model.provider;
    let mut text_block_index = 0usize;

    for block in content {
        match block {
            ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => {
                if same_model {
                    // Verbatim replay; the signature IS the reasoning item.
                    if let Some(sig) = signature
                        && let Ok(item) = serde_json::from_str::<JsonValue>(sig)
                    {
                        items.push(InputItem::Reasoning(item));
                    }
                } else if !thinking.trim().is_empty() {
                    // A foreign reasoning item cannot be replayed; its text
                    // still informs the new model as plain assistant text.
                    push_text_item(items, thinking, None, msg_index, &mut text_block_index);
                }
            }
            ContentBlock::Text { text, signature } => {
                // Cross-model replay drops the text identity: the id and
                // phase belong to the other model's turn.
                let signature = if same_model {
                    signature.as_deref()
                } else {
                    None
                };
                push_text_item(items, text, signature, msg_index, &mut text_block_index);
            }
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                let effective = if same_model {
                    id.clone()
                } else {
                    id_map
                        .entry(id.clone())
                        .or_insert_with(|| {
                            normalize_tool_call_id(id, &context.model.provider, msg_provider)
                        })
                        .clone()
                };

                let mut parts = effective.split('|');
                let call_id = parts.next().unwrap_or("").to_string();
                let item_id = parts.next();
                // The item id pairs the call with the server's reasoning
                // bookkeeping. A same-turn fc_ id keeps that pairing; a
                // different model's fc_ id would fail validation against
                // reasoning state that is not being replayed, and a
                // non-fc_ id is malformed on this wire — both are omitted.
                let item_id = match item_id {
                    Some(item) if item.starts_with("fc_") && !is_different_model => {
                        Some(item.to_string())
                    }
                    _ => None,
                };
                items.push(InputItem::Item(OutputItem::FunctionCall {
                    call_id,
                    name: name.clone(),
                    arguments: input.to_string(),
                    id: item_id,
                }));
            }
            // Redacted thinking is opaque to every provider but its origin;
            // images never appear in assistant turns on this shape.
            _ => {}
        }
    }
}

/// Push one assistant text block as a replayed `message` output item. The
/// item id comes from the block's signature when present, else a
/// deterministic positional fallback; overlong ids hash down to the 64-char
/// limit.
fn push_text_item(
    items: &mut Vec<InputItem>,
    text: &str,
    signature: Option<&str>,
    msg_index: usize,
    text_block_index: &mut usize,
) {
    let parsed = signature.map(parse_text_signature);
    let fallback = if *text_block_index == 0 {
        format!("msg_pi_{msg_index}")
    } else {
        format!("msg_pi_{msg_index}_{text_block_index}")
    };
    *text_block_index += 1;
    let (id, phase) = match parsed {
        Some((id, phase)) if id.len() > 64 => (format!("msg_{}", short_hash(&id)), phase),
        Some((id, phase)) => (id, phase),
        None => (fallback, None),
    };
    items.push(InputItem::Item(OutputItem::Message {
        id,
        role: "assistant",
        content: vec![OutputTextPart {
            kind: "output_text",
            text: text.to_string(),
            annotations: Vec::new(),
        }],
        status: "completed",
        phase,
    }));
}

fn user_part(block: &ContentBlock) -> Option<InputPart> {
    match block {
        ContentBlock::Text { text, .. } => Some(InputPart::Text { text: text.clone() }),
        ContentBlock::Image { data, mime_type } => {
            let url = format!("data:{mime_type};base64,{data}");
            Some(InputPart::Image {
                image_url: url,
                detail: "auto",
            })
        }
        _ => None,
    }
}

/// A tool result splits into its text and any images; placeholders keep the
/// output non-empty, which the API requires. The error bit has no wire
/// representation on this shape.
fn tool_result_output(blocks: &[ContentBlock]) -> FunctionOutput {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } => texts.push(text.as_str()),
            ContentBlock::Image { data, mime_type } => {
                let url = format!("data:{mime_type};base64,{data}");
                images.push(InputPart::Image {
                    image_url: url,
                    detail: "auto",
                });
            }
            _ => {}
        }
    }
    let joined = texts.join("\n");
    if images.is_empty() {
        return FunctionOutput::Text(if joined.is_empty() {
            "(no tool output)".to_string()
        } else {
            joined
        });
    }
    let mut parts: Vec<InputPart> = Vec::new();
    if !joined.is_empty() {
        parts.push(InputPart::Text { text: joined });
    } else {
        parts.push(InputPart::Text {
            text: "(see attached image)".to_string(),
        });
    }
    parts.extend(images);
    FunctionOutput::Parts(parts)
}

// ── Ids and signatures ──────────────────────────────────────────────────────

/// Providers whose Responses endpoints issue `call_id|item_id` tool call
/// ids; anything else gets its ids normalized as opaque foreign strings.
const RESPONSES_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];

/// Normalize a tool call id for replay onto a Responses endpoint. Ids in
/// the `call_id|item_id` scheme keep their structure; item ids must carry
/// the `fc_` prefix, and a foreign item id hashes down to a fresh `fc_` id
/// (the original pairs with the other provider's reasoning state).
fn normalize_tool_call_id(id: &str, model_provider: &str, source_provider: &str) -> String {
    if !RESPONSES_TOOL_CALL_PROVIDERS.contains(&model_provider) {
        return normalize_id_part(id);
    }
    let mut parts = id.split('|');
    let call_id = parts.next().unwrap_or("");
    let Some(item_id) = parts.next() else {
        return normalize_id_part(id);
    };
    let call_id = normalize_id_part(call_id);
    let item_id = if source_provider != model_provider {
        let hashed = format!("fc_{}", short_hash(item_id));
        hashed.chars().take(64).collect::<String>()
    } else {
        normalize_id_part(item_id)
    };
    let item_id = if item_id.starts_with("fc_") {
        item_id
    } else {
        normalize_id_part(&format!("fc_{item_id}"))
    };
    format!("{call_id}|{item_id}")
}

/// An id part is ASCII alphanumerics, `_` and `-`, at most 64 chars, and
/// does not trail off with fillers.
fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let truncated: String = sanitized.chars().take(64).collect();
    truncated.trim_end_matches('_').to_string()
}

/// The text signature encodes the item identity a text block had on the
/// wire: `{v:1, id, phase?}`. A legacy signature is a bare id string.
pub fn encode_text_signature(id: &str, phase: Option<&str>) -> String {
    let mut payload = serde_json::json!({ "v": 1, "id": id });
    if let Some(phase) = phase {
        payload["phase"] = JsonValue::String(phase.to_string());
    }
    payload.to_string()
}

/// Decode a text signature into `(id, phase)`. Unknown phases drop — the
/// API validates the enum, so an unrecognized value must not be replayed.
pub fn parse_text_signature(signature: &str) -> (String, Option<String>) {
    if signature.starts_with('{')
        && let Ok(v) = serde_json::from_str::<JsonValue>(signature)
        && v["v"] == 1
        && let Some(id) = v["id"].as_str()
    {
        let phase = match v["phase"].as_str() {
            Some("commentary") => Some("commentary".to_string()),
            Some("final_answer") => Some("final_answer".to_string()),
            _ => None,
        };
        return (id.to_string(), phase);
    }
    (signature.to_string(), None)
}

/// Deterministic short hash (cyrb53) used to compact overlong or foreign
/// ids. Stability across requests and processes is the contract — the same
/// input must always yield the same id.
pub fn short_hash(s: &str) -> String {
    let mut h1: u32 = 0xdead_beef;
    let mut h2: u32 = 0x41c6_ce57;
    for unit in s.encode_utf16() {
        h1 = (h1 ^ unit as u32).wrapping_mul(2654435761);
        h2 = (h2 ^ unit as u32).wrapping_mul(1597334677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2246822507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3266489909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2246822507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3266489909);
    format!("{}{}", to_base36(h2), to_base36(h1))
}

fn to_base36(mut n: u32) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while n > 0 {
        let d = (n % 36) as u8;
        digits.push(if d < 10 { b'0' + d } else { b'a' + (d - 10) });
        n /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("base36 digits are ASCII")
}

// ── Usage ───────────────────────────────────────────────────────────────────

/// Fold the wire usage into the domain `Usage`.
///
/// The wire's `input_tokens` includes the cached and cache-write subsets,
/// so the domain's non-cached `input_tokens` subtracts both — otherwise
/// `total_input()` would count them twice. `total_tokens` is the wire's own
/// total, taken verbatim.
pub fn to_usage(wire: &WireUsage) -> crate::types::Usage {
    let (cached, written) = match &wire.input_tokens_details {
        Some(d) => (
            d.cached_tokens.unwrap_or(0),
            d.cache_write_tokens.unwrap_or(0),
        ),
        None => (0, 0),
    };
    crate::types::Usage {
        input_tokens: wire
            .input_tokens
            .unwrap_or(0)
            .saturating_sub(cached + written),
        output_tokens: wire.output_tokens.unwrap_or(0),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: written,
        cache_write_1h: None,
        total_tokens: wire.total_tokens.unwrap_or(0),
        reasoning_tokens: wire
            .output_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens),
        cost: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentMessage, ContentBlock, Model, ThinkingKind, Usage};
    use serde_json::json;
    use std::sync::Arc;

    fn model(thinking: ThinkingKind) -> Model {
        Model {
            provider: "openai".into(),
            id: "gpt-test".into(),
            api: "openai_responses".into(),
            context_window: 200_000,
            max_tokens: 16_384,
            thinking,
            metadata: Default::default(),
        }
    }

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(text)
    }

    /// An assistant message from the model under test (same-model replay).
    fn assistant(content: Vec<ContentBlock>) -> AgentMessage {
        assistant_from(content, "openai", "gpt-test")
    }

    /// An assistant message from another model/provider in the history.
    fn assistant_from(content: Vec<ContentBlock>, provider: &str, model_id: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content,
            model: model_id.into(),
            provider: provider.into(),
            api: "openai_responses".into(),
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
            tool_name: "Read".into(),
            content,
            is_error,
            details: None,
            usage: None,
            added_tool_names: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn text(s: &str) -> ContentBlock {
        ContentBlock::Text {
            text: s.into(),
            signature: None,
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
            stream_options: Default::default(),
        }
    }

    fn request(ctx: &AgentContext) -> serde_json::Value {
        serde_json::to_value(to_request(ctx, &StreamOptions::default())).unwrap()
    }

    // ── reasoning params ────────────────────────────────────────────────────

    #[test]
    fn level_turns_reasoning_on_regardless_of_model_metadata() {
        for kind in [
            ThinkingKind::Enabled,
            ThinkingKind::Adaptive,
            ThinkingKind::None,
        ] {
            let v = request(&ctx(vec![user("hi")], kind, Some("high")));
            assert_eq!(
                v["reasoning"],
                json!({"effort": "high", "summary": "auto"}),
                "{kind:?}"
            );
            assert_eq!(
                v["include"],
                json!(["reasoning.encrypted_content"]),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn off_or_absent_level_is_explicit_off() {
        for level in [Some("off"), None] {
            let v = request(&ctx(vec![user("hi")], ThinkingKind::Enabled, level));
            assert_eq!(v["reasoning"], json!({"effort": "none"}), "{level:?}");
            assert!(v.get("include").is_none(), "{level:?}");
        }
    }

    // ── request shell ───────────────────────────────────────────────────────
    #[test]
    fn request_shell_fields() {
        let opts = StreamOptions {
            max_tokens: Some(4),
            ..Default::default()
        };
        let v = serde_json::to_value(to_request(
            &ctx(vec![user("hi")], ThinkingKind::None, None),
            &opts,
        ))
        .unwrap();
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        // The API floor for max_output_tokens is 16.
        assert_eq!(v["max_output_tokens"], 16);

        let opts = StreamOptions {
            max_tokens: Some(1000),
            ..Default::default()
        };
        let v = serde_json::to_value(to_request(
            &ctx(vec![user("hi")], ThinkingKind::None, None),
            &opts,
        ))
        .unwrap();
        assert_eq!(v["max_output_tokens"], 1000);

        let v = request(&ctx(vec![user("hi")], ThinkingKind::None, None));
        assert!(v.get("max_output_tokens").is_none());
    }

    #[test]
    fn cache_fields() {
        let mut c = ctx(vec![user("hi")], ThinkingKind::None, None);
        c.session_id = Some("s".repeat(100));

        // Default (short) retention: key sent, clamped to 64 chars; no
        // retention field.
        let v = serde_json::to_value(to_request(&c, &StreamOptions::default())).unwrap();
        assert_eq!(v["prompt_cache_key"].as_str().unwrap().len(), 64);
        assert!(v.get("prompt_cache_retention").is_none());

        // Long retention adds the 24h retention field.
        c.cache_retention = crate::types::CacheRetention::Long;
        let v = serde_json::to_value(to_request(&c, &StreamOptions::default())).unwrap();
        assert_eq!(v["prompt_cache_retention"], "24h");

        // Retention off sends neither field.
        c.cache_retention = crate::types::CacheRetention::None;
        let v = serde_json::to_value(to_request(&c, &StreamOptions::default())).unwrap();
        assert!(v.get("prompt_cache_key").is_none());
        assert!(v.get("prompt_cache_retention").is_none());
    }

    // ── system prompt ───────────────────────────────────────────────────────

    #[test]
    fn system_prompt_role_follows_thinking_capability() {
        let v = request(&ctx(vec![user("hi")], ThinkingKind::Enabled, Some("high")));
        assert_eq!(
            v["input"][0],
            json!({"role": "developer", "content": "sys"})
        );

        let v = request(&ctx(vec![user("hi")], ThinkingKind::None, None));
        assert_eq!(v["input"][0], json!({"role": "system", "content": "sys"}));
    }

    // ── user messages ───────────────────────────────────────────────────────

    #[test]
    fn user_message_uses_parts_encoding() {
        let msg = AgentMessage::User {
            content: vec![
                text("what is this"),
                ContentBlock::Image {
                    data: "AAAA".into(),
                    mime_type: "image/png".into(),
                },
            ],
            timestamp: chrono::Utc::now(),
        };
        let v = request(&ctx(vec![msg], ThinkingKind::None, None));
        let content = &v["input"][1]["content"];
        assert_eq!(
            content[0],
            json!({"type": "input_text", "text": "what is this"})
        );
        assert_eq!(
            content[1],
            json!({"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "auto"})
        );
    }

    // ── assistant text replay ───────────────────────────────────────────────

    #[test]
    fn same_model_text_replays_with_signature_identity() {
        let sig = encode_text_signature("msg_abc", Some("final_answer"));
        let msg = assistant(vec![ContentBlock::Text {
            text: "answer".into(),
            signature: Some(sig),
        }]);
        let v = request(&ctx(vec![user("q"), msg], ThinkingKind::None, None));
        let item = &v["input"][2];
        assert_eq!(item["type"], "message");
        assert_eq!(item["id"], "msg_abc");
        assert_eq!(item["role"], "assistant");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["phase"], "final_answer");
        assert_eq!(
            item["content"][0],
            json!({"type": "output_text", "text": "answer", "annotations": []})
        );
    }

    #[test]
    fn cross_model_text_falls_back_to_positional_id() {
        let sig = encode_text_signature("msg_abc", Some("commentary"));
        let msg = assistant_from(
            vec![ContentBlock::Text {
                text: "answer".into(),
                signature: Some(sig),
            }],
            "openai",
            "gpt-other",
        );
        let v = request(&ctx(vec![user("q"), msg], ThinkingKind::None, None));
        let item = &v["input"][2];
        // The foreign identity is dropped: positional fallback, no phase.
        assert_eq!(item["id"], "msg_pi_1");
        assert!(item.get("phase").is_none());
    }

    #[test]
    fn overlong_signature_id_hashes_down() {
        let long_id = "m".repeat(100);
        let sig = encode_text_signature(&long_id, None);
        let msg = assistant(vec![ContentBlock::Text {
            text: "a".into(),
            signature: Some(sig),
        }]);
        let v = request(&ctx(vec![user("q"), msg], ThinkingKind::None, None));
        let id = v["input"][2]["id"].as_str().unwrap();
        assert_eq!(id, format!("msg_{}", short_hash(&long_id)));
        assert!(id.len() <= 64);
    }

    // ── thinking replay ─────────────────────────────────────────────────────

    #[test]
    fn same_model_thinking_replays_raw_reasoning_item() {
        let item = json!({
            "id": "rs_1",
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "hmm"}],
            "encrypted_content": "enc1"
        });
        let msg = assistant(vec![
            ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: Some(item.to_string()),

                redacted: None,
            },
            text("answer"),
        ]);
        let v = request(&ctx(
            vec![user("q"), msg],
            ThinkingKind::Enabled,
            Some("high"),
        ));
        // The reasoning item is byte-identical to the captured signature.
        assert_eq!(v["input"][2], item);
        assert_eq!(v["input"][3]["type"], "message");
    }

    #[test]
    fn unsigned_thinking_drops_and_empties_the_turn() {
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
        let kinds: Vec<&str> = v["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["role"].as_str().unwrap_or("<item>"))
            .collect();
        assert_eq!(kinds, ["system", "user", "user"]);
    }

    #[test]
    fn cross_model_thinking_flattens_to_text() {
        let msg = assistant_from(
            vec![ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: Some(r#"{"id":"rs_foreign","type":"reasoning","summary":[]}"#.into()),

                redacted: None,
            }],
            "openai",
            "gpt-other",
        );
        let v = request(&ctx(vec![user("q"), msg], ThinkingKind::None, None));
        let item = &v["input"][2];
        // The foreign reasoning item must not be replayed; its text survives
        // as plain assistant text.
        assert_eq!(item["type"], "message");
        assert_eq!(item["content"][0]["text"], "hmm");
    }

    // ── tool calls and results ──────────────────────────────────────────────

    #[test]
    fn same_model_tool_call_keeps_fc_item_id() {
        let msg = assistant(vec![ContentBlock::ToolUse {
            id: "call_1|fc_item1".into(),
            name: "Read".into(),
            input: json!({"path": "x"}),
            thought_signature: None,
        }]);
        let v = request(&ctx(vec![user("q"), msg], ThinkingKind::None, None));
        let item = &v["input"][2];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "call_1");
        assert_eq!(item["id"], "fc_item1");
        assert_eq!(item["name"], "Read");
        assert_eq!(item["arguments"], "{\"path\":\"x\"}");
    }

    #[test]
    fn different_model_tool_call_drops_item_id_and_remaps_result() {
        let msg = assistant_from(
            vec![ContentBlock::ToolUse {
                id: "call_1|fc_item1".into(),
                name: "Read".into(),
                input: json!({}),
                thought_signature: None,
            }],
            "openai",
            "gpt-other",
        );
        let v = request(&ctx(
            vec![
                user("q"),
                msg,
                tool_result("call_1|fc_item1", vec![text("ok")], false),
            ],
            ThinkingKind::None,
            None,
        ));
        let call = &v["input"][2];
        assert_eq!(call["type"], "function_call");
        assert_eq!(call["call_id"], "call_1");
        // Another model's fc_ id fails pairing validation; omit it.
        assert!(call.get("id").is_none());
        // The result follows the normalized call id.
        let output = &v["input"][3];
        assert_eq!(output["type"], "function_call_output");
        assert_eq!(output["call_id"], "call_1");
        assert_eq!(output["output"], "ok");
    }

    #[test]
    fn foreign_provider_tool_call_hashes_item_id() {
        let msg = assistant_from(
            vec![ContentBlock::ToolUse {
                id: "call_1|fc_item1".into(),
                name: "Read".into(),
                input: json!({}),
                thought_signature: None,
            }],
            "anthropic",
            "claude-x",
        );
        let v = request(&ctx(vec![user("q"), msg], ThinkingKind::None, None));
        let call = &v["input"][2];
        assert_eq!(call["call_id"], "call_1");
        // A cross-provider fc_ id hashes into a fresh, well-formed one.
        let id = call["id"].as_str().unwrap();
        assert_eq!(id, format!("fc_{}", short_hash("fc_item1")));
    }

    #[test]
    fn anthropic_style_tool_call_id_normalizes_whole() {
        let msg = assistant_from(
            vec![ContentBlock::ToolUse {
                id: "toolu_01AbC".into(),
                name: "Read".into(),
                input: json!({}),
                thought_signature: None,
            }],
            "anthropic",
            "claude-x",
        );
        let v = request(&ctx(
            vec![
                user("q"),
                msg,
                tool_result("toolu_01AbC", vec![text("ok")], false),
            ],
            ThinkingKind::None,
            None,
        ));
        let call = &v["input"][2];
        assert_eq!(call["call_id"], "toolu_01AbC");
        assert!(call.get("id").is_none());
        assert_eq!(v["input"][3]["call_id"], "toolu_01AbC");
    }

    #[test]
    fn tool_result_output_variants_without_error_fold() {
        // Plain text joins blocks.
        let v = request(&ctx(
            vec![
                user("q"),
                tool_result("c1", vec![text("a"), text("b")], false),
            ],
            ThinkingKind::None,
            None,
        ));
        assert_eq!(v["input"][2]["output"], "a\nb");

        // The error bit has no wire representation on this shape.
        let v = request(&ctx(
            vec![user("q"), tool_result("c1", vec![text("boom")], true)],
            ThinkingKind::None,
            None,
        ));
        assert_eq!(v["input"][2]["output"], "boom");

        // Empty content takes the placeholder.
        let v = request(&ctx(
            vec![user("q"), tool_result("c1", Vec::new(), false)],
            ThinkingKind::None,
            None,
        ));
        assert_eq!(v["input"][2]["output"], "(no tool output)");

        // Images switch the output to parts.
        let image = ContentBlock::Image {
            data: "AAAA".into(),
            mime_type: "image/png".into(),
        };
        let v = request(&ctx(
            vec![user("q"), tool_result("c1", vec![image], false)],
            ThinkingKind::None,
            None,
        ));
        let output = &v["input"][2]["output"];
        assert_eq!(
            output[0],
            json!({"type": "input_text", "text": "(see attached image)"})
        );
        assert_eq!(
            output[1],
            json!({"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "auto"})
        );
    }

    #[test]
    fn orphaned_tool_call_gains_synthetic_error_output() {
        let msg = assistant(vec![ContentBlock::ToolUse {
            id: "call_1|fc_1".into(),
            name: "Read".into(),
            input: json!({}),
            thought_signature: None,
        }]);
        // No tool result between the call and the next user message.
        let v = request(&ctx(
            vec![user("q"), msg, user("next")],
            ThinkingKind::None,
            None,
        ));
        let output = &v["input"][3];
        assert_eq!(output["type"], "function_call_output");
        assert_eq!(output["call_id"], "call_1");
        assert_eq!(output["output"], "No result provided");
        assert_eq!(v["input"][4]["role"], "user");
    }

    #[test]
    fn custom_message_reaches_the_request_as_user_text() {
        let custom = AgentMessage::Custom {
            custom_type: "note".into(),
            content: vec![ContentBlock::Text {
                text: "remember this".into(),
                signature: None,
            }],
            display: false,
            details: None,
            timestamp: chrono::Utc::now(),
        };
        let v = request(&ctx(vec![custom], ThinkingKind::None, None));
        let input = v["input"].as_array().unwrap();
        let last = input.last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["text"], "remember this");
    }

    // ── usage ───────────────────────────────────────────────────────────────

    #[test]
    fn usage_subtracts_cached_and_write() {
        let wire = WireUsage {
            input_tokens: Some(1000),
            output_tokens: Some(50),
            total_tokens: Some(1050),
            input_tokens_details: Some(WireInputTokensDetails {
                cached_tokens: Some(700),
                cache_write_tokens: Some(100),
            }),
            output_tokens_details: None,
        };
        let u = to_usage(&wire);
        assert_eq!(u.input_tokens, 200);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cache_read_input_tokens, 700);
        assert_eq!(u.cache_creation_input_tokens, 100);
        assert_eq!(u.total_input(), 1000);
        // The wire total is taken verbatim.
        assert_eq!(u.total_tokens, 1050);

        // A malformed payload with subsets exceeding the total saturates.
        let bad = WireUsage {
            input_tokens: Some(10),
            output_tokens: None,
            total_tokens: None,
            input_tokens_details: Some(WireInputTokensDetails {
                cached_tokens: Some(99),
                cache_write_tokens: None,
            }),
            output_tokens_details: None,
        };
        assert_eq!(to_usage(&bad).input_tokens, 0);
    }

    // ── signatures and ids ──────────────────────────────────────────────────

    #[test]
    fn text_signature_round_trip() {
        let sig = encode_text_signature("msg_1", Some("commentary"));
        let (id, phase) = parse_text_signature(&sig);
        assert_eq!(id, "msg_1");
        assert_eq!(phase.as_deref(), Some("commentary"));

        let sig = encode_text_signature("msg_2", None);
        let (id, phase) = parse_text_signature(&sig);
        assert_eq!(id, "msg_2");
        assert_eq!(phase, None);

        // A legacy signature is a bare id string.
        let (id, phase) = parse_text_signature("msg_plain");
        assert_eq!(id, "msg_plain");
        assert_eq!(phase, None);

        // An unknown phase value drops rather than replays invalid data.
        let (id, phase) = parse_text_signature(r#"{"v":1,"id":"m","phase":"weird"}"#);
        assert_eq!(id, "m");
        assert_eq!(phase, None);
    }

    #[test]
    fn short_hash_matches_reference_vectors() {
        assert_eq!(short_hash(""), "k4n83c7h0j2b");
        assert_eq!(short_hash("fc_abc123"), "ia5uw610gdqii");
        assert_eq!(
            short_hash(&format!("msg_{}", "x".repeat(100))),
            "19ce5491y36to1"
        );
        assert_eq!(short_hash("call_1234|fc_item"), "giekm16a6r65");
        assert_eq!(short_hash("toolu_01AbC"), "di6khg1qi4mhq");
    }

    #[test]
    fn normalize_id_part_sanitizes_truncates_and_trims() {
        assert_eq!(normalize_id_part("call_abc-123"), "call_abc-123");
        // Special characters become underscores, trailing ones trim off.
        assert_eq!(normalize_id_part("call|abc!!!"), "call_abc");
        assert_eq!(normalize_id_part(&"x".repeat(100)), "x".repeat(64));
    }
}
