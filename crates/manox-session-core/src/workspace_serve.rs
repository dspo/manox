//! Serving side of the durable Workspace domain (deepseek-harness
//! workspace-controller parity): the process-singleton store, the wire
//! conversions, and the verb dispatch behind `ClientCall::Workspace`.
//!
//! The store is the authority; this module never caches domain state. A
//! session whose directory backs no workspace row stays loose (listed
//! outside any folder account), exactly like dsh.

use manox_protocol::workspace::{WorkspaceCall, WorkspaceWire, WorkspaceWireEvent};
use manox_workspace::{WorkspaceError, WorkspaceEvent, WorkspaceStore, WorkspaceView};
use serde_json::{Value, json};

/// The process-singleton registry (crate-owned sqlite under the manox home).
pub fn store() -> &'static WorkspaceStore {
    static STORE: std::sync::OnceLock<WorkspaceStore> = std::sync::OnceLock::new();
    STORE.get_or_init(|| {
        WorkspaceStore::open().expect("workspace registry opens under the manox home")
    })
}

pub fn wire_view(view: &WorkspaceView) -> WorkspaceWire {
    WorkspaceWire {
        workspace_id: view.workspace_id.clone(),
        path: view.path.clone(),
        title: view.title.clone(),
        session_ids: view.session_ids.clone(),
        created_at: view.created_at.clone(),
        updated_at: view.updated_at.clone(),
    }
}

pub fn wire_event(event: &WorkspaceEvent) -> WorkspaceWireEvent {
    match event {
        WorkspaceEvent::Baseline {
            workspaces,
            archived_session_ids,
        } => WorkspaceWireEvent::Baseline {
            workspaces: workspaces.iter().map(wire_view).collect(),
            archived_session_ids: archived_session_ids.clone(),
        },
        WorkspaceEvent::Upsert { workspace } => WorkspaceWireEvent::Upsert {
            workspace: wire_view(workspace),
        },
        WorkspaceEvent::Remove { workspace_id } => WorkspaceWireEvent::Remove {
            workspace_id: workspace_id.clone(),
        },
        WorkspaceEvent::Order { workspace_ids } => WorkspaceWireEvent::Order {
            workspace_ids: workspace_ids.clone(),
        },
        WorkspaceEvent::Archived {
            archived_session_ids,
        } => WorkspaceWireEvent::Archived {
            archived_session_ids: archived_session_ids.clone(),
        },
    }
}

fn error_value(error: WorkspaceError) -> manox_protocol::RpcError {
    manox_protocol::RpcError::new(-1, error.to_string())
}

/// Dispatch one workspace verb; responses are whole wire values.
pub async fn call(call: WorkspaceCall) -> Result<Value, manox_protocol::RpcError> {
    let store = store();
    match call {
        WorkspaceCall::List => {
            let views = store.list().map_err(error_value)?;
            Ok(json!({
                "workspaces": views.iter().map(wire_view).collect::<Vec<_>>(),
                "archivedSessionIds": store_archived(),
            }))
        }
        WorkspaceCall::Create { path } => {
            let (view, created) = store
                .create(std::path::Path::new(&path))
                .map_err(error_value)?;
            Ok(json!({ "workspace": wire_view(&view), "created": created }))
        }
        WorkspaceCall::Rename {
            workspace_id,
            title,
        } => {
            let view = store.rename(&workspace_id, &title).map_err(error_value)?;
            Ok(json!({ "workspace": wire_view(&view) }))
        }
        WorkspaceCall::Delete { workspace_id } => {
            store.delete(&workspace_id).map_err(error_value)?;
            Ok(json!({ "deleted": true }))
        }
        WorkspaceCall::InsertBefore {
            workspace_id,
            before,
        } => {
            let order = store
                .insert_before(&workspace_id, before.as_deref())
                .map_err(error_value)?;
            Ok(json!({ "workspaceIds": order }))
        }
        WorkspaceCall::AttachSession {
            workspace_id,
            session_id,
        } => {
            let view = store
                .attach_session(&workspace_id, &session_id)
                .map_err(error_value)?;
            Ok(json!({ "workspace": wire_view(&view) }))
        }
        WorkspaceCall::InsertSessionBefore {
            workspace_id,
            session_id,
            before,
        } => {
            let view = store
                .insert_session_before(&workspace_id, &session_id, before.as_deref())
                .map_err(error_value)?;
            Ok(json!({ "workspace": wire_view(&view) }))
        }
        WorkspaceCall::DetachSession {
            workspace_id,
            session_id,
        } => {
            let view = store
                .detach_session(&workspace_id, &session_id)
                .map_err(error_value)?;
            Ok(json!({ "workspace": wire_view(&view) }))
        }
        WorkspaceCall::ArchiveSession {
            session_id,
            archived,
        } => {
            let archived_ids = store
                .archive_session(&session_id, archived)
                .map_err(error_value)?;
            Ok(json!({ "archivedSessionIds": archived_ids }))
        }
        WorkspaceCall::Status { workspace_id } => {
            let status = store.status(&workspace_id).map_err(error_value)?;
            Ok(json!({ "status": status }))
        }
    }
}

fn store_archived() -> Vec<String> {
    match store().baseline_event() {
        WorkspaceEvent::Baseline {
            archived_session_ids,
            ..
        } => archived_session_ids,
        _ => Vec::new(),
    }
}

/// Account a fresh session to the workspace row backing `project` (dsh:
/// prepend at attach). A directory with no row leaves the session loose.
pub fn attach_if_member(project: &str, session_id: &str) {
    let store = store();
    let Ok(canon) = std::fs::canonicalize(project) else {
        return;
    };
    let Ok(rows) = store.list() else {
        return;
    };
    if let Some(row) = rows.into_iter().find(|r| r.path == canon.to_string_lossy())
        && let Err(error) = store.attach_session(&row.workspace_id, session_id)
    {
        tracing::debug!(%error, "workspace attach skipped");
    }
}

/// Bind hand-off bookkeeping: the bound directory becomes (or joins) a
/// workspace row and the successor session leads its account.
pub fn create_and_attach(directory: &str, session_id: &str) {
    let store = store();
    match store.create(std::path::Path::new(directory)) {
        Ok((view, _)) => {
            if let Err(error) = store.attach_session(&view.workspace_id, session_id) {
                tracing::debug!(%error, "workspace attach after bind skipped");
            }
        }
        Err(error) => tracing::debug!(%error, "workspace create after bind skipped"),
    }
}

/// First-boot adoption retry loop: the thread store may still be scanning
/// when the server comes up; adoption is idempotent through the
/// `initialized` flag, so a late pass is harmless.
pub async fn adopt_when_ready() {
    for _ in 0..10 {
        if manox_agent::thread_store::try_global().is_some() {
            match store().adopt_from_thread_store() {
                Ok(adopted) => {
                    if adopted > 0 {
                        tracing::info!(adopted, "workspace adoption derived rows");
                    }
                    return;
                }
                Err(error) => {
                    tracing::warn!(%error, "workspace adoption failed");
                    return;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
