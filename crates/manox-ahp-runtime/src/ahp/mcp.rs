//! The AHP face of the process-global MCP registry.
//!
//! MCP servers are process-global in manox but per-session in AHP
//! (`SessionState.customizations`), so the registry snapshot is mirrored into
//! every session: the seed overlays it and the event pump republishes each
//! transition as `session/mcpServerStateChanged`, keeping a late subscriber's
//! baseline and a live subscriber's stream sourced from the same registry.
//! The registry's ready servers are also the session's `serverTools` — the
//! same event pump keeps that list in step.
//!
//! Ready servers expose the `mcp://` side-channel (`channel`), proxying the
//! `tools/*` and `resources/*` method families to the upstream server; the
//! advertisement rides the customization's `mcpApp` capability flags. Not
//! served: MCP Apps `ui/*`, `sampling`, and the `authRequired` lifecycle
//! (manox resolves credentials outside the protocol — file-configured
//! headers, keychain keys — so no OAuth challenge ever reaches the AHP face).

use ahp_types::actions::SessionMcpServerStateChangedAction;
use ahp_types::state::{
    AhpMcpUiHostCapabilities, ErrorInfo, McpServerCustomization, McpServerCustomizationApps,
    McpServerErrorState, McpServerReadyState, McpServerState, McpServerStoppedState,
    ToolDefinition,
};

/// The `mcp://` side-channel URI of one server. The registry is
/// process-global, so the channel is keyed by the server's registry name and
/// is the same string from every session that mirrors the customization.
pub fn channel_uri(server: &str) -> String {
    format!("mcp://{server}")
}

/// The side-channel advertisement for a ready server: `tools/*` and
/// `resources/*` are proxied (the flags are presence markers — the empty
/// object means "served").
fn ready_apps() -> McpServerCustomizationApps {
    McpServerCustomizationApps {
        capabilities: AhpMcpUiHostCapabilities {
            server_tools: Some(serde_json::json!({})),
            server_resources: Some(serde_json::json!({})),
            logging: None,
            sampling: None,
        },
    }
}

/// Every configured server as a session customization, in registry order.
/// An uninitialized registry contributes nothing — MCP is optional. Ready
/// servers carry their side-channel; Error/Stopped clear it.
pub fn customizations() -> Vec<McpServerCustomization> {
    let Some(registry) = manox_agent::mcp::try_global() else {
        return Vec::new();
    };
    registry
        .slots()
        .into_iter()
        .map(|slot| {
            let ready = slot.state == manox_agent::mcp::ServerState::Ready;
            let channel = ready.then(|| channel_uri(&slot.name));
            McpServerCustomization {
                id: slot.name.clone(),
                uri: slot.source_uri,
                name: slot.name,
                icons: None,
                range: None,
                meta: None,
                enablement: None,
                state: ahp_state(&slot.state),
                channel,
                mcp_app: ready.then(ready_apps),
            }
        })
        .collect()
}

/// The `session/mcpServerStateChanged` action a registry transition maps to.
/// Full replacement of the entry's state; the side-channel rides along —
/// present while ready, cleared otherwise.
pub fn state_changed(
    event: manox_agent::mcp::McpServerEvent,
) -> SessionMcpServerStateChangedAction {
    let ready = event.state == manox_agent::mcp::ServerState::Ready;
    SessionMcpServerStateChangedAction {
        id: event.name.clone(),
        state: ahp_state(&event.state),
        channel: ready.then(|| channel_uri(&event.name)),
    }
}

/// The session's `serverTools`: the tool inventory of every ready server,
/// flattened. The MCP tool set is exactly what the model sees through the
/// `mcp__<server>__<tool>` bridge, so the list a client renders here matches
/// what a turn can actually call.
pub fn server_tools() -> Vec<ToolDefinition> {
    let Some(registry) = manox_agent::mcp::try_global() else {
        return Vec::new();
    };
    registry
        .servers()
        .into_iter()
        .flat_map(|server| server.tools.into_iter().map(|tool| tool_definition(&tool)))
        .collect()
}

/// `rmcp::model::Tool` → wire `ToolDefinition`.
fn tool_definition(tool: &rmcp::model::Tool) -> ToolDefinition {
    ToolDefinition {
        name: tool.name.to_string(),
        title: tool.title.clone(),
        description: tool.description.as_ref().map(|d| d.to_string()),
        input_schema: Some(serde_json::Value::Object((*tool.input_schema).clone())),
        output_schema: tool
            .output_schema
            .as_ref()
            .map(|schema| serde_json::Value::Object((**schema).clone())),
        annotations: tool
            .annotations
            .as_ref()
            .map(|a| ahp_types::state::ToolAnnotations {
                title: a.title.clone(),
                read_only_hint: a.read_only_hint,
                destructive_hint: a.destructive_hint,
                idempotent_hint: a.idempotent_hint,
                open_world_hint: a.open_world_hint,
            }),
        meta: None,
    }
}

fn ahp_state(state: &manox_agent::mcp::ServerState) -> McpServerState {
    match state {
        manox_agent::mcp::ServerState::Ready => McpServerState::Ready(McpServerReadyState {}),
        manox_agent::mcp::ServerState::Error(message) => {
            McpServerState::Error(McpServerErrorState {
                error: ErrorInfo {
                    error_type: "mcp".to_string(),
                    message: message.clone(),
                    stack: None,
                    meta: None,
                },
            })
        }
        manox_agent::mcp::ServerState::Stopped => McpServerState::Stopped(McpServerStoppedState {}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manox_agent::mcp::{McpServerEvent, ServerState};

    #[test]
    fn a_ready_slot_maps_to_a_ready_customization_keyed_by_name() {
        let action = state_changed(McpServerEvent {
            name: "github".to_string(),
            state: ServerState::Ready,
        });
        assert_eq!(action.id, "github");
        assert_eq!(action.state, McpServerState::Ready(McpServerReadyState {}));
        assert_eq!(action.channel.as_deref(), Some("mcp://github"));
    }

    #[test]
    fn an_error_slot_carries_the_message_and_clears_the_channel() {
        let action = state_changed(McpServerEvent {
            name: "fs".to_string(),
            state: ServerState::Error("connect timed out".to_string()),
        });
        let McpServerState::Error(state) = action.state else {
            panic!("expected error state");
        };
        assert_eq!(state.error.error_type, "mcp");
        assert_eq!(state.error.message, "connect timed out");
        assert_eq!(action.channel, None);
    }

    #[test]
    fn a_stopped_slot_maps_to_stopped() {
        let action = state_changed(McpServerEvent {
            name: "fs".to_string(),
            state: ServerState::Stopped,
        });
        assert_eq!(
            action.state,
            McpServerState::Stopped(McpServerStoppedState {})
        );
        assert_eq!(action.channel, None);
    }

    #[test]
    fn channel_round_trips_through_the_canonical_parser() {
        use manox_ahp::channels::mcp;
        let uri = channel_uri("fs");
        assert_eq!(mcp::server(&uri), Some("fs"));
        assert!(mcp::serves("tools/list"));
    }

    #[test]
    fn an_rmcp_tool_maps_onto_the_wire_tool_definition() {
        use std::sync::Arc;
        let schema: serde_json::Map<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )
        .unwrap();
        let wire_schema = serde_json::Value::Object(schema.clone());
        let tool = rmcp::model::Tool::new("read_file", "Read a file", Arc::new(schema));
        let wire = tool_definition(&tool);
        assert_eq!(wire.name, "read_file");
        assert_eq!(wire.description.as_deref(), Some("Read a file"));
        assert_eq!(wire.input_schema, Some(wire_schema));
        assert_eq!(wire.output_schema, None);
        assert_eq!(wire.annotations, None);
    }
}
