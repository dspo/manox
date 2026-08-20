//! Host-side `AgentBus` — the unified inter-agent messaging + spawn
//! mechanism. Routes `Steer` messages between `User`, the singleton
//! `Captain`, and dynamically-spawned `Subagent` instances. Sits on the
//! kernel's intra-session `steer` (`HarnessHandle::steer`). TS Pi has no
//! cross-session agent bus — this is a manox host extension.
//!
//! Phase A: skeleton only (types + struct + stub methods). Full routing,
//! spawn, and completion logic land in Phase D.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use pi::ext_point_agent::AgentDef;
use pi::harness::HarnessHandle;
use pi::tool::{AgentToolResult, ToolError};
use pi_extensions::steer_bus::{AgentId, SteerPayload, SteerReason, ToSpec};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::thread_engine::BackendNotice;

/// A live in-thread subagent coroutine (transient, not a manox Thread).
pub struct LiveSubagent {
    pub handle: HarnessHandle,
    pub def: AgentDef,
    pub parent: AgentId,
    pub cancel: CancellationToken,
}

/// Parent routing info injected into a member thread's bus so the member
/// can address its parent Captain by thread id.
#[derive(Clone)]
pub struct ParentRoute {
    pub parent_thread_id: String,
    pub parent_notice_tx: mpsc::UnboundedSender<BackendNotice>,
}

/// The host-side agent bus. One per thread (Captain or member). The
/// Captain's bus tracks live subagents + spawned member thread-ids; a
/// member's bus carries a `parent_route` to address its parent.
// Fields are read in Phase D (steer implementation); Phase A is skeleton.
#[allow(dead_code)]
pub struct AgentBus {
    owner_thread_id: String,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    live_subagents: Mutex<BTreeMap<String, LiveSubagent>>,
    spawned_members: Mutex<HashSet<String>>,
    parent_route: Mutex<Option<ParentRoute>>,
    task_list: Mutex<Option<Arc<Mutex<crate::team::TaskList>>>>,
    captain_handle: Mutex<Option<HarnessHandle>>,
}

impl AgentBus {
    pub fn new(owner_thread_id: String, notice_tx: mpsc::UnboundedSender<BackendNotice>) -> Self {
        Self {
            owner_thread_id,
            notice_tx,
            live_subagents: Mutex::new(BTreeMap::new()),
            spawned_members: Mutex::new(HashSet::new()),
            parent_route: Mutex::new(None),
            task_list: Mutex::new(None),
            captain_handle: Mutex::new(None),
        }
    }

    /// Late-bind the Captain's session handle (session is built after
    /// `build_tools`, so the handle isn't available at construction).
    pub fn bind_captain(&self, handle: HarnessHandle) {
        *self.captain_handle.lock().unwrap() = Some(handle);
    }

    /// Inject parent routing into a member thread's bus.
    pub fn set_parent_route(&self, route: ParentRoute) {
        *self.parent_route.lock().unwrap() = Some(route);
    }

    /// Inject the shared TaskList (Arc-cloned from the parent bus).
    pub fn set_task_list(&self, list: Arc<Mutex<crate::team::TaskList>>) {
        *self.task_list.lock().unwrap() = Some(list);
    }

    /// The main Steer routing entry point.
    /// Phase A: stub — full implementation in Phase D.
    pub async fn steer(
        &self,
        from: AgentId,
        to: ToSpec,
        reason: SteerReason,
        payload: SteerPayload,
    ) -> Result<AgentToolResult, ToolError> {
        let _ = (from, to, reason, payload);
        Err(ToolError::ExecutionFailed(
            "AgentBus::steer not implemented yet (Phase A skeleton)".into(),
        ))
    }
}
