//! Anthropic Messages API prompt caching (`cache_control` breakpoints).
//!
//! The strategy is chosen by provider capability (`PromptCachingPolicy`):
//! - `Full`: system last block + last tool + messages[-2] + messages[-1], up to
//!   4 breakpoints. For providers that honor all breakpoints (real Anthropic etc.).
//! - `LastBreakpointOnly`: last tool only (stable prefix system+tools). For
//!   providers that only respect the last breakpoint (third-party Anthropic
//!   compatible endpoints) — placing a breakpoint on messages[-1] (which
//!   changes every turn) causes a rewrite every turn with zero reads (net
//!   negative); placing only the tool breakpoint yields reads every turn with
//!   zero creation.
//! - `None`: no caching.
//!
//! The default policy is decided by `resolve_prompt_caching_policy` against the
//! endpoint host: an explicit `prompt_caching` config value wins; otherwise
//! `api.anthropic.com` → Full, anything else → LastBreakpointOnly (conservative;
//! third-party anthropic-compatible endpoints often have breakpoint
//! restrictions). This mirrors the philosophy of "full config for the
//! official endpoint, conservative for third parties".

use serde_json::{Value, json};

/// Anthropic allows at most 4 `cache_control` breakpoints per request.
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Beta header enabling the extended (1h) cache TTL on real Anthropic.
pub const EXTENDED_CACHE_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";

/// A short-lived ephemeral breakpoint: `{type:"ephemeral"}` (5min, no `ttl`).
/// When `long_ttl` is true the breakpoint carries `ttl:"1h"` — only honored by
/// real Anthropic; third-party endpoints typically ignore `ttl` so it is only
/// set when the policy is `Full` and the endpoint is the official API.
pub fn ephemeral_cache_control(long_ttl: bool) -> Value {
    if long_ttl {
        json!({"type": "ephemeral", "ttl": "1h"})
    } else {
        json!({"type": "ephemeral"})
    }
}

/// Prompt caching policy, decided by provider capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptCachingPolicy {
    /// No caching.
    #[default]
    None,
    /// Full 4-breakpoint layout: system last block + last tool + messages[-2] + messages[-1].
    Full,
    /// Last tool breakpoint only (stable prefix system+tools).
    LastBreakpointOnly,
}

impl PromptCachingPolicy {
    /// Parse from a config string ("none"/"full"/"last_breakpoint").
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "none" => Some(Self::None),
            "full" => Some(Self::Full),
            "last_breakpoint" | "last-breakpoint" => Some(Self::LastBreakpointOnly),
            _ => None,
        }
    }
}

/// Resolve the policy from provider capability: explicit config `prompt_caching`
/// wins; otherwise by `base_url` host — `api.anthropic.com` → Full (maxed out),
/// anything else → LastBreakpointOnly (conservative). `base_url` of `None` is
/// treated conservatively.
pub fn resolve_prompt_caching_policy(
    prompt_caching: Option<&str>,
    base_url: Option<&str>,
) -> PromptCachingPolicy {
    if let Some(s) = prompt_caching
        && let Some(p) = PromptCachingPolicy::parse(s)
    {
        return p;
    }
    match endpoint_host(base_url).as_deref() {
        Some("api.anthropic.com") => PromptCachingPolicy::Full,
        _ => PromptCachingPolicy::LastBreakpointOnly,
    }
}

/// Whether long (1h) TTL is appropriate: only on real Anthropic, i.e. the `Full`
/// policy against the `api.anthropic.com` host. The caller adds the
/// `extended-cache-ttl` beta header when this returns true.
pub fn supports_long_ttl(policy: PromptCachingPolicy, base_url: Option<&str>) -> bool {
    policy == PromptCachingPolicy::Full
        && endpoint_host(base_url).as_deref() == Some("api.anthropic.com")
}

/// Extract the host from a base URL string, tolerating missing scheme.
fn endpoint_host(base_url: Option<&str>) -> Option<String> {
    let url = base_url?;
    // reqwest re-exports `url::Url`; prefer it when a scheme is present.
    if let Ok(parsed) = reqwest::Url::parse(url) {
        return parsed.host_str().map(str::to_string);
    }
    // Fallback for scheme-less inputs: strip a leading `scheme://` if present,
    // then take the part before the first '/'/':'.
    let no_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = no_scheme.split(['/', ':']).next().unwrap_or("");
    (!host.is_empty()).then(|| host.to_string())
}

/// Place `cache_control` breakpoints on `request` (an Anthropic Messages wire
/// body) according to `policy`. The caller must have already built the body
/// (`model`, `max_tokens`, `messages`, optional `system` string, optional
/// `tools` array). `long_ttl` selects `ttl:"1h"` vs the default 5min.
///
/// Assumes `body["system"]` is either a `Value::String` or absent, and
/// `body["tools"]` is a `Value::Array` of `Value::Object` tool defs or absent.
pub fn apply_prompt_caching(body: &mut Value, policy: PromptCachingPolicy, long_ttl: bool) {
    if policy == PromptCachingPolicy::None {
        return;
    }
    let cc = ephemeral_cache_control(long_ttl);
    let mut used = 0usize;

    // (1) System last block (Full only): upgrade a string system to a single
    // text block carrying cache_control.
    if policy == PromptCachingPolicy::Full
        && used < MAX_CACHE_BREAKPOINTS
        && let Some(system) = body.get_mut("system")
    {
        if system.is_string() {
            let text = system.as_str().unwrap_or("").to_string();
            *system = json!([{"type": "text", "text": text, "cache_control": cc}]);
            used += 1;
        } else if let Some(blocks) = system.as_array_mut()
            && let Some(last) = blocks.last_mut()
            && let Some(map) = last.as_object_mut()
            && !map.contains_key("cache_control")
        {
            map.insert("cache_control".to_string(), cc.clone());
            used += 1;
        }
    }

    // (2) Last tool (Full and LastBreakpointOnly): cache the system+tools
    // prefix that precedes it.
    if used < MAX_CACHE_BREAKPOINTS
        && policy != PromptCachingPolicy::None
        && let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut())
        && let Some(last) = tools.last_mut()
        && let Some(map) = last.as_object_mut()
        && !map.contains_key("cache_control")
    {
        map.insert("cache_control".to_string(), cc.clone());
        used += 1;
    }

    // (3)(4) messages[-2], messages[-1] last text block (Full only).
    if policy == PromptCachingPolicy::Full
        && let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut())
    {
        let len = messages.len();
        let start = len.saturating_sub(2);
        for i in start..len {
            if used >= MAX_CACHE_BREAKPOINTS {
                break;
            }
            if let Some(msg) = messages.get_mut(i).and_then(|m| m.as_object_mut())
                && let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut())
            {
                for block in blocks.iter_mut().rev() {
                    let is_cacheable = block
                        .as_object()
                        .and_then(|m| m.get("type"))
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t == "text" || t == "tool_result");
                    if !is_cacheable {
                        continue;
                    }
                    if let Some(bm) = block.as_object_mut()
                        && !bm.contains_key("cache_control")
                    {
                        bm.insert("cache_control".to_string(), cc.clone());
                        used += 1;
                        break;
                    }
                }
            }
        }
    }

    // Safety net: if the upstream pre-seeded breakpoints, trim to the cap.
    enforce_cache_control_limit(body, MAX_CACHE_BREAKPOINTS);
}

/// Count `cache_control` keys across system blocks, tools, and message content blocks.
fn count_breakpoints(body: &Value) -> usize {
    let mut total = 0;
    if let Some(Value::Array(blocks)) = body.get("system") {
        total += blocks
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .count();
    }
    if let Some(Value::Array(tools)) = body.get("tools") {
        total += tools
            .iter()
            .filter(|b| matches!(b, Value::Object(m) if m.contains_key("cache_control")))
            .count();
    }
    if let Some(Value::Array(messages)) = body.get("messages") {
        for msg in messages {
            if let Some(Value::Array(blocks)) = msg.get("content") {
                total += blocks
                    .iter()
                    .filter(|b| matches!(b, Value::Object(m) if m.contains_key("cache_control")))
                    .count();
            }
        }
    }
    total
}

/// Strip excess breakpoints (keeping the last system and last tool breakpoint,
/// then message breakpoints from the end) when the total exceeds `max`.
fn enforce_cache_control_limit(body: &mut Value, max: usize) {
    let total = count_breakpoints(body);
    if total <= max {
        return;
    }
    let mut excess = total - max;

    // Strip all but the last system breakpoint.
    if let Some(blocks) = body.get_mut("system").and_then(|v| v.as_array_mut()) {
        let last_cc = blocks
            .iter()
            .rposition(|b| b.get("cache_control").is_some());
        strip_except_index(blocks, last_cc, &mut excess);
    }
    if excess == 0 {
        return;
    }

    // Strip all but the last tool breakpoint.
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        let last_cc = tools
            .iter()
            .rposition(|b| matches!(b, Value::Object(m) if m.contains_key("cache_control")));
        strip_except_index(tools, last_cc, &mut excess);
    }
    if excess == 0 {
        return;
    }

    // Strip message breakpoints from the end.
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut().rev() {
            if excess == 0 {
                break;
            }
            if let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                for block in blocks.iter_mut().rev() {
                    if excess == 0 {
                        break;
                    }
                    if let Some(m) = block.as_object_mut()
                        && m.remove("cache_control").is_some()
                    {
                        excess -= 1;
                    }
                }
            }
        }
    }
}

/// Remove `cache_control` from every entry except `preserve_index`, decrementing `excess`.
fn strip_except_index(blocks: &mut [Value], preserve_index: Option<usize>, excess: &mut usize) {
    for (i, block) in blocks.iter_mut().enumerate() {
        if *excess == 0 {
            return;
        }
        if Some(i) == preserve_index {
            continue;
        }
        if let Value::Object(m) = block
            && m.remove("cache_control").is_some()
        {
            *excess -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body_with(system: Option<Value>, tools: Vec<Value>, messages: Vec<Value>) -> Value {
        let mut body = json!({
            "model": "claude-test",
            "max_tokens": 1024,
            "messages": messages,
            "stream": true,
        });
        if let Some(s) = system {
            body["system"] = s;
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        body
    }

    fn user_text_msg(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}]})
    }

    #[test]
    fn full_places_4_breakpoints() {
        let system = Some(Value::String("sys".into()));
        let tools = vec![
            json!({"name": "a", "input_schema": {}}),
            json!({"name": "b", "input_schema": {}}),
        ];
        let messages = vec![user_text_msg("first"), user_text_msg("second")];
        let mut req = body_with(system, tools, messages);
        apply_prompt_caching(&mut req, PromptCachingPolicy::Full, false);

        // system upgraded to blocks with cache_control.
        assert!(matches!(req.get("system"), Some(Value::Array(_))));
        assert!(req["system"][0].get("cache_control").is_some());
        // last tool carries cache_control, first does not.
        assert!(
            matches!(req["tools"].as_array().unwrap().last(), Some(Value::Object(m)) if m.contains_key("cache_control"))
        );
        assert!(
            !matches!(req["tools"].as_array().unwrap().first(), Some(Value::Object(m)) if m.contains_key("cache_control"))
        );
        assert_eq!(count_breakpoints(&req), 4);
    }

    #[test]
    fn last_breakpoint_only_places_tool_only() {
        let system = Some(Value::String("sys".into()));
        let tools = vec![
            json!({"name": "a", "input_schema": {}}),
            json!({"name": "b", "input_schema": {}}),
        ];
        let messages = vec![user_text_msg("first"), user_text_msg("second")];
        let mut req = body_with(system, tools, messages);
        apply_prompt_caching(&mut req, PromptCachingPolicy::LastBreakpointOnly, false);

        // system stays a string, messages untouched, only last tool carries cc.
        assert!(matches!(req.get("system"), Some(Value::String(_))));
        assert!(
            matches!(req["tools"].as_array().unwrap().last(), Some(Value::Object(m)) if m.contains_key("cache_control"))
        );
        assert_eq!(count_breakpoints(&req), 1);
    }

    #[test]
    fn none_leaves_request_untouched() {
        let mut req = body_with(
            Some(Value::String("sys".into())),
            vec![json!({"name": "a", "input_schema": {}})],
            vec![user_text_msg("hi")],
        );
        apply_prompt_caching(&mut req, PromptCachingPolicy::None, false);
        assert_eq!(count_breakpoints(&req), 0);
    }

    #[test]
    fn long_ttl_adds_1h() {
        let mut req = body_with(
            Some(Value::String("sys".into())),
            vec![json!({"name": "a", "input_schema": {}})],
            vec![user_text_msg("hi")],
        );
        apply_prompt_caching(&mut req, PromptCachingPolicy::Full, true);
        let cc = req["system"][0]["cache_control"].as_object().unwrap();
        assert_eq!(cc["type"], json!("ephemeral"));
        assert_eq!(cc["ttl"], json!("1h"));
    }

    #[test]
    fn ephemeral_short_has_no_ttl() {
        let cc = ephemeral_cache_control(false);
        assert_eq!(cc, json!({"type": "ephemeral"}));
    }

    #[test]
    fn resolve_policy_config_overrides_base_url() {
        assert_eq!(
            resolve_prompt_caching_policy(Some("full"), Some("https://dashscope/v1")),
            PromptCachingPolicy::Full
        );
        assert_eq!(
            resolve_prompt_caching_policy(Some("none"), Some("https://api.anthropic.com")),
            PromptCachingPolicy::None
        );
    }

    #[test]
    fn resolve_policy_default_by_base_url() {
        assert_eq!(
            resolve_prompt_caching_policy(None, Some("https://api.anthropic.com")),
            PromptCachingPolicy::Full
        );
        assert_eq!(
            resolve_prompt_caching_policy(
                None,
                Some("https://dashscope.aliyuncs.com/apps/anthropic")
            ),
            PromptCachingPolicy::LastBreakpointOnly
        );
        assert_eq!(
            resolve_prompt_caching_policy(None, None),
            PromptCachingPolicy::LastBreakpointOnly
        );
    }

    #[test]
    fn resolve_policy_unknown_config_falls_back_to_base_url() {
        assert_eq!(
            resolve_prompt_caching_policy(Some("garbage"), Some("https://api.anthropic.com")),
            PromptCachingPolicy::Full
        );
    }

    #[test]
    fn enforces_4_cap_when_upstream_pre_seeded() {
        // 5 messages each pre-seeded with cache_control.
        let messages: Vec<Value> = (0..5)
            .map(|i| {
                json!({"role": "user", "content": [{"type": "text", "text": format!("m{i}"), "cache_control": {"type": "ephemeral"}}]})
            })
            .collect();
        let mut req = body_with(None, vec![], messages);
        apply_prompt_caching(&mut req, PromptCachingPolicy::Full, false);
        assert!(count_breakpoints(&req) <= MAX_CACHE_BREAKPOINTS);
    }

    #[test]
    fn supports_long_ttl_only_full_official() {
        assert!(supports_long_ttl(
            PromptCachingPolicy::Full,
            Some("https://api.anthropic.com")
        ));
        assert!(!supports_long_ttl(
            PromptCachingPolicy::Full,
            Some("https://dashscope.aliyuncs.com")
        ));
        assert!(!supports_long_ttl(
            PromptCachingPolicy::LastBreakpointOnly,
            Some("https://api.anthropic.com")
        ));
    }

    #[test]
    fn endpoint_host_handles_schemeless() {
        assert_eq!(
            endpoint_host(Some("api.anthropic.com")),
            Some("api.anthropic.com".into())
        );
        assert_eq!(
            endpoint_host(Some("https://api.anthropic.com/v1")),
            Some("api.anthropic.com".into())
        );
        assert_eq!(endpoint_host(None), None);
    }
}
