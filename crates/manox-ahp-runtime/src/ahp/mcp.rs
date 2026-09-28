//! The AHP face of the process-global MCP registry.
//!
//! MCP servers are process-global in manox but per-session in AHP
//! (`SessionState.customizations`), so the registry snapshot is mirrored into
//! every session: the seed overlays it and the event pump republishes each
//! transition as `session/mcpServerStateChanged`, keeping a late subscriber's
//! baseline and a live subscriber's stream sourced from the same registry.
//!
//! Not served: the `mcp://` side-channel (`channel` stays `None` — the
//! registry has no per-client MCP proxy) and the `authRequired` lifecycle
//! (manox resolves provider credentials outside the protocol, so no OAuth
//! challenge ever reaches the AHP face).

use ahp_types::actions::SessionMcpServerStateChangedAction;
use ahp_types::state::{
    ErrorInfo, McpServerCustomization, McpServerErrorState, McpServerReadyState, McpServerState,
    McpServerStoppedState,
};

/// Every configured server as a session customization, in registry order.
/// An uninitialized registry contributes nothing — MCP is optional.
pub fn customizations() -> Vec<McpServerCustomization> {
    let Some(registry) = manox_agent::mcp::try_global() else {
        return Vec::new();
    };
    registry
        .slots()
        .into_iter()
        .map(|slot| McpServerCustomization {
            id: slot.name.clone(),
            uri: slot.source_uri,
            name: slot.name,
            icons: None,
            range: None,
            meta: None,
            enablement: None,
            state: ahp_state(&slot.state),
            channel: None,
            mcp_app: None,
        })
        .collect()
}

/// The `session/mcpServerStateChanged` action a registry transition maps to.
/// Full replacement of the entry's state; no side-channel exists to carry.
pub fn state_changed(
    event: manox_agent::mcp::McpServerEvent,
) -> SessionMcpServerStateChangedAction {
    SessionMcpServerStateChangedAction {
        id: event.name,
        state: ahp_state(&event.state),
        channel: None,
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
        assert_eq!(action.channel, None);
    }

    #[test]
    fn an_error_slot_carries_the_message_in_an_error_info() {
        let action = state_changed(McpServerEvent {
            name: "fs".to_string(),
            state: ServerState::Error("connect timed out".to_string()),
        });
        let McpServerState::Error(state) = action.state else {
            panic!("expected error state");
        };
        assert_eq!(state.error.error_type, "mcp");
        assert_eq!(state.error.message, "connect timed out");
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
    }
}
