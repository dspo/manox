//! Workspace namespace vocabulary (deepseek-harness workspace-controller
//! parity): the verb set clients drive the durable Workspace domain with,
//! and the reconnect-safe state-stream frames the host broadcasts.
//!
//! Wire shapes mirror `manox_workspace`'s domain types 1:1; the conversion
//! lives in manox-session-core (the protocol crate stays dependency-free of
//! the domain crate).

use serde::{Deserialize, Serialize};

/// One client→host workspace verb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "camelCase")]
pub enum WorkspaceCall {
    List,
    Create {
        path: String,
    },
    Rename {
        workspace_id: String,
        title: String,
    },
    Delete {
        workspace_id: String,
    },
    /// DOM-insertBefore-like display-order move over workspace rows.
    InsertBefore {
        workspace_id: String,
        before: Option<String>,
    },
    AttachSession {
        workspace_id: String,
        session_id: String,
    },
    InsertSessionBefore {
        workspace_id: String,
        session_id: String,
        before: Option<String>,
    },
    DetachSession {
        workspace_id: String,
        session_id: String,
    },
    ArchiveSession {
        session_id: String,
        archived: bool,
    },
    Status {
        workspace_id: String,
    },
}

/// One durable workspace row on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceWire {
    pub workspace_id: String,
    pub path: String,
    pub title: String,
    pub session_ids: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Reconnect-safe state-stream frame (baseline on attach, increments
/// broadcast on every accepted mutation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkspaceWireEvent {
    Baseline {
        workspaces: Vec<WorkspaceWire>,
        archived_session_ids: Vec<String>,
    },
    Upsert {
        workspace: WorkspaceWire,
    },
    Remove {
        workspace_id: String,
    },
    Order {
        workspace_ids: Vec<String>,
    },
    Archived {
        archived_session_ids: Vec<String>,
    },
}
