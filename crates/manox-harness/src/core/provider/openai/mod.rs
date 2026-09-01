// OpenAI-compatible providers, one submodule per API shape.
//
// `completions` covers the Chat Completions protocol spoken by OpenAI and
// most OpenAI-compatible endpoints; `responses` covers the Responses
// protocol spoken by OpenAI's newer models.
//
// Compatibility policy: the request side encodes exactly what the caller
// declared — `ThinkingKind` selects the thinking wire mechanism and effort
// levels pass through unclamped, so an unsupported combination fails at the
// vendor rather than being second-guessed here. The response side parses
// liberally: extension fields that endpoints invented independently
// (`reasoning_content` and its spelling variants, the flat cache-hit
// counters) are deserialized unconditionally — present-when-relevant,
// harmless when absent. Behavior branches on the endpoint only in the two
// helpers below, each a documented protocol requirement of the endpoint.

pub mod completions;
pub mod responses;

/// Endpoints whose Chat Completions implementation only accepts the legacy
/// `max_tokens` field name; everything else takes `max_completion_tokens`.
fn uses_legacy_max_tokens(provider: &str, base_url: &str) -> bool {
    provider == "moonshotai"
        || provider == "moonshotai-cn"
        || provider == "together"
        || provider == "nvidia"
        || provider == "cloudflare-ai-gateway"
        || provider == "ant-ling"
        || base_url.contains("chutes.ai")
        || base_url.contains("api.moonshot.")
        || base_url.contains("api.together.")
        || base_url.contains("gateway.ai.cloudflare.com")
        || base_url.contains("integrate.api.nvidia.com")
        || base_url.contains("api.ant-ling.com")
}

/// Endpoints whose thinking mode rejects a replayed assistant message that
/// lacks the `reasoning_content` field; an empty string satisfies them.
fn requires_reasoning_content_on_assistant(provider: &str, base_url: &str) -> bool {
    provider == "deepseek" || base_url.contains("deepseek.com")
}

/// `prompt_cache_key` is capped at 64 characters.
fn clamp_cache_key(key: &str) -> String {
    key.chars().take(64).collect()
}

/// Strict OpenAI-compatible endpoints (DashScope's Responses API among them)
/// reject object schemas that omit `properties`, which schemars never emits
/// for empty structs. Object schemas therefore always carry the key on the
/// wire; the empty map preserves the declared semantics exactly.
fn ensure_object_properties(mut schema: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = schema.as_object_mut()
        && obj.get("type").and_then(|t| t.as_str()) == Some("object")
        && !obj.contains_key("properties")
    {
        obj.insert("properties".into(), serde_json::Map::new().into());
    }
    schema
}

#[cfg(test)]
mod tests {
    use super::ensure_object_properties;
    use serde_json::json;

    #[test]
    fn object_schema_without_properties_gains_empty_map() {
        let schema = json!({ "type": "object", "additionalProperties": false });
        let fixed = ensure_object_properties(schema);
        assert_eq!(fixed["properties"], json!({}));
        assert_eq!(fixed["additionalProperties"], json!(false));
    }

    #[test]
    fn declared_properties_pass_through_untouched() {
        let schema = json!({ "type": "object", "properties": { "a": { "type": "string" } } });
        assert_eq!(ensure_object_properties(schema.clone()), schema);
    }

    #[test]
    fn non_object_schemas_pass_through_untouched() {
        let schema = json!({ "type": "string" });
        assert_eq!(ensure_object_properties(schema.clone()), schema);
    }
}
