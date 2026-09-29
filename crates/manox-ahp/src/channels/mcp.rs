//! The `mcp://` side-channel — MCP JSON-RPC spoken verbatim over the AHP
//! transport, routed by the request's `channel` param instead of the method
//! table (the method *is* the upstream MCP method, e.g. `tools/list`).
//!
//! The registry is process-global, so the channel is keyed by the server's
//! registry name: `mcp://<server-key>`. Served surface: the `tools/*` and
//! `resources/*` method families — exactly what the customization's
//! capability flags advertise. Everything else on this scheme answers
//! `MethodNotFound`, per the side-channel spec's gating rule.

use serde_json::Value;

/// The MCP method families this host proxies. Anything else arriving on an
/// `mcp://` channel is refused before it reaches the runtime.
pub const SERVED_METHODS: &[&str] = &[
    "tools/list",
    "tools/call",
    "resources/list",
    "resources/templates/list",
    "resources/read",
];

/// The server key inside an `mcp://<key>` channel URI.
pub fn server(uri: &str) -> Option<&str> {
    uri.strip_prefix("mcp://").filter(|key| !key.is_empty())
}

/// Whether `method` is in the served capability set.
pub fn serves(method: &str) -> bool {
    SERVED_METHODS.contains(&method)
}

/// Strip the AHP routing envelope (`channel`) off the params, leaving the
/// upstream MCP request params for `serde` to decode into the rmcp types.
pub fn upstream_params(params: &mut Value) {
    if let Some(object) = params.as_object_mut() {
        object.remove("channel");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn server_key_round_trips() {
        assert_eq!(server("mcp://github"), Some("github"));
        assert_eq!(server("mcp://"), None);
        assert_eq!(server("ahp-session:/s-1"), None);
        assert_eq!(server("mcp://my-server.dev"), Some("my-server.dev"));
    }

    #[test]
    fn the_served_set_is_exactly_the_advertised_families() {
        for method in [
            "tools/list",
            "tools/call",
            "resources/list",
            "resources/templates/list",
            "resources/read",
        ] {
            assert!(serves(method), "{method} must be served");
        }
        for method in [
            "sampling/createMessage",
            "logging/setLevel",
            "ping",
            "initialize",
        ] {
            assert!(!serves(method), "{method} must not be served");
        }
    }

    #[test]
    fn upstream_params_strips_only_the_envelope() {
        let mut params = json!({"channel": "mcp://fs", "cursor": "abc", "_meta": {"a": 1}});
        upstream_params(&mut params);
        assert_eq!(
            params,
            json!({"cursor": "abc", "_meta": {"a": 1}}),
            "the channel key is the AHP envelope; the rest is upstream MCP"
        );
    }
}
