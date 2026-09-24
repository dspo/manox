//! The gateway's implementation of the AHP runtime seam.
//!
//! [`SessionRuntime`] is what the AHP adapter speaks; this module is where those
//! 22 methods land on the live session gateway. The adapter itself never names a
//! gateway type — it holds a `dyn SessionRuntime` — so the two are coupled at
//! exactly this file, and deleting the v2 wire half later cannot reach the
//! adapter.
//!
//! **Every method here is a delegation, not a reimplementation.** The gateway's
//! `AgentServerInner` intents are the one write path v2 and AHP have always
//! shared (the same property `ahp_inner()` was introduced for); this adapter
//! exists to give them a name the AHP side can hold without depending on the
//! gateway's crate layout.

use std::sync::Arc;

use manox_ahp_runtime::error::RuntimeError;
use manox_ahp_runtime::runtime_trait::{
    ForkIntent, RenameOutcome, SessionIntent, SessionRuntime, TerminalSnapshot,
};
use serde_json::Value;

use crate::agent_server::AgentServer;

/// Run an async runtime seam from the host's synchronous `Backend` methods.
///
/// The host calls these from an async context, so the block has to yield the
/// worker rather than park it (`block_in_place`) — the same discipline the
/// adapter's own helper follows, and the reason a bare `handle().block_on`
/// panics here.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| manox_agent::runtime::handle().block_on(future))
}

/// The gateway as the AHP runtime.
pub struct GatewayRuntime {
    server: Arc<AgentServer>,
}

impl GatewayRuntime {
    /// Wrap a live server.
    pub fn new(server: Arc<AgentServer>) -> Self {
        Self { server }
    }

    /// The wrapped server, for the gateway's own wiring (the `/ahp` route and
    /// the v2 legs both need to reach it).
    pub fn server(&self) -> &Arc<AgentServer> {
        &self.server
    }
}

impl SessionRuntime for GatewayRuntime {
    fn terminal_state(&self, terminal_id: &str) -> Option<TerminalSnapshot> {
        #[cfg(feature = "terminal")]
        {
            let state = self.server.ahp_inner().ahp_terminal_state(terminal_id)?;
            let lines = match state.content.first() {
                Some(ahp_types::state::TerminalContentPart::Unclassified(part)) => {
                    part.value.split('\n').map(str::to_string).collect()
                }
                _ => Vec::new(),
            };
            let exit_code = match state.lifecycle {
                ahp_types::state::TerminalLifecycleState::Exited(exited) => exited.exit_code,
                ahp_types::state::TerminalLifecycleState::Running(_) => None,
            };
            let session_id = match state.claim {
                ahp_types::state::TerminalClaim::Session(claim) => {
                    manox_ahp::channels::session::id(&claim.session)
                        .map(str::to_string)
                        .unwrap_or_default()
                }
                ahp_types::state::TerminalClaim::Client(_) => String::new(),
            };
            Some(TerminalSnapshot {
                title: state.title,
                cwd: state.cwd,
                cols: state.cols.unwrap_or_default(),
                rows: state.rows.unwrap_or_default(),
                lines,
                exit_code,
                session_id,
            })
        }
        #[cfg(not(feature = "terminal"))]
        {
            let _ = terminal_id;
            None
        }
    }

    fn terminal_raw_tap(
        &self,
        terminal_id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<std::sync::Arc<Vec<u8>>>> {
        #[cfg(feature = "terminal")]
        {
            self.server.ahp_inner().terminal_raw_tap(terminal_id)
        }
        #[cfg(not(feature = "terminal"))]
        {
            let _ = terminal_id;
            None
        }
    }

    fn create_terminal(
        &self,
        session_id: &str,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), RuntimeError> {
        #[cfg(feature = "terminal")]
        {
            self.server
                .ahp_inner()
                .attach_terminal(session_id, cols, rows, Some(terminal_id.to_string()))
                .map(|_| ())
                .map_err(|error| RuntimeError::new(error.message))
        }
        #[cfg(not(feature = "terminal"))]
        {
            let _ = (session_id, terminal_id, cols, rows);
            Err(RuntimeError::new(
                "terminal support is not built into this host",
            ))
        }
    }

    fn dispose_terminal(&self, terminal_id: &str) -> Result<(), RuntimeError> {
        #[cfg(feature = "terminal")]
        {
            self.server
                .ahp_inner()
                .dispose_terminal(terminal_id)
                .then_some(())
                .ok_or_else(|| RuntimeError::new(format!("unknown terminal {terminal_id}")))
        }
        #[cfg(not(feature = "terminal"))]
        {
            let _ = terminal_id;
            Err(RuntimeError::new(
                "terminal support is not built into this host",
            ))
        }
    }

    fn terminal_input(&self, terminal_id: &str, data: &str) -> Result<(), RuntimeError> {
        #[cfg(feature = "terminal")]
        {
            self.server
                .ahp_inner()
                .terminal_input(terminal_id, data)
                .map_err(RuntimeError::new)
        }
        #[cfg(not(feature = "terminal"))]
        {
            let _ = (terminal_id, data);
            Err(RuntimeError::new(
                "terminal support is not built into this host",
            ))
        }
    }

    fn terminal_resize(&self, terminal_id: &str, cols: u16, rows: u16) -> Result<(), RuntimeError> {
        #[cfg(feature = "terminal")]
        {
            self.server
                .ahp_inner()
                .terminal_resize(terminal_id, cols, rows)
                .map_err(RuntimeError::new)
        }
        #[cfg(not(feature = "terminal"))]
        {
            let _ = (terminal_id, cols, rows);
            Err(RuntimeError::new(
                "terminal support is not built into this host",
            ))
        }
    }

    fn create_session(&self, owner: &str, intent: SessionIntent) -> Result<(), RuntimeError> {
        use crate::agent_server::AgentServerInner;
        let server = Arc::clone(&self.server);
        let inner = Arc::clone(server.ahp_inner());
        let owner = owner.to_string();
        block_on(async move {
            AgentServerInner::create_session_request(&inner, &owner, intent).await
        })
        .map(|_| ())
        .map_err(|error| crate::agent_server::preserve_code(error))
    }

    fn fork_session(&self, owner: &str, intent: ForkIntent) -> Result<(), RuntimeError> {
        let server = Arc::clone(&self.server);
        let inner = Arc::clone(server.ahp_inner());
        let owner = owner.to_string();
        block_on(
            async move { crate::agent_server::fork_session(&inner, &owner, intent).await },
        )
        .map(|_| ())
        .map_err(|error| crate::agent_server::preserve_code(error))
    }

    fn dispose_session(&self, owner: &str, session_id: &str) -> Result<(), RuntimeError> {
        self.server.ahp_inner().dispose_session(owner, session_id);
        Ok(())
    }

    fn has_session(&self, session_id: &str) -> bool {
        self.server.ahp_inner().session_thread(session_id).is_some()
    }

    fn submit(&self, owner: &str, session_id: &str, text: String) -> Result<Value, RuntimeError> {
        let server = Arc::clone(&self.server);
        let inner = Arc::clone(server.ahp_inner());
        let owner = owner.to_string();
        let session_id = session_id.to_string();
        block_on(async move {
            inner
                .submit(&owner, &session_id, text, Vec::new(), None, None)
                .await
        })
        .map_err(|error| crate::agent_server::preserve_code(error))
    }

    fn steer(
        &self,
        session_id: &str,
        message_id: &str,
        text: String,
    ) -> Result<Value, RuntimeError> {
        self.server
            .ahp_inner()
            .steer(session_id, message_id.to_string(), text, Vec::new(), None)
            .map_err(|error| crate::agent_server::preserve_code(error))
    }

    fn drop_queued(&self, session_id: &str, message_id: &str) {
        self.server
            .ahp_inner()
            .drop_queued(session_id, message_id.to_string());
    }

    fn cancel_turn(&self, session_id: &str) -> Result<(), RuntimeError> {
        match self.server.ahp_inner().session_thread(session_id) {
            Some(thread) => {
                thread.with_mut(|t| t.cancel());
                Ok(())
            }
            None => Err(RuntimeError::new(format!("unknown session {session_id}"))
                .with_code(manox_ahp_runtime::error::codes::SESSION_NOT_FOUND)),
        }
    }

    fn set_model(&self, session_id: &str, model: &str) -> Result<(), RuntimeError> {
        self.server.ahp_inner().set_model(session_id, model)
    }

    fn set_reasoning_effort(&self, session_id: &str, effort: &str) -> Result<(), RuntimeError> {
        self.server
            .ahp_inner()
            .set_reasoning_effort(session_id, effort)
    }

    fn set_approval_mode(&self, session_id: &str, mode: &str) -> Result<(), RuntimeError> {
        self.server.ahp_inner().set_approval_mode(session_id, mode)
    }

    fn set_cwd(&self, session_id: &str, cwd: &str) -> Result<(), RuntimeError> {
        let server = Arc::clone(&self.server);
        let inner = Arc::clone(server.ahp_inner());
        let session_id = session_id.to_string();
        let cwd = cwd.to_string();
        block_on(async move { inner.set_cwd(&session_id, &cwd).await })
    }

    fn archive_session(&self, owner: &str, session_id: &str, archived: bool) {
        self.server
            .ahp_inner()
            .archive_thread(owner, session_id, archived);
    }

    fn rename_session(&self, session_id: &str, title: &str) -> RenameOutcome {
        match self.server.ahp_inner().rename_thread(session_id, title) {
            manox_agent::thread_store::RenameOutcome::Renamed => RenameOutcome::Renamed,
            manox_agent::thread_store::RenameOutcome::Blank => RenameOutcome::Blank,
            manox_agent::thread_store::RenameOutcome::UnknownSession => {
                RenameOutcome::UnknownSession
            }
            manox_agent::thread_store::RenameOutcome::NotPersisted => RenameOutcome::NotPersisted,
        }
    }

    fn pin_session(&self, session_id: &str, pinned: bool) -> bool {
        self.server.ahp_inner().pin_session(session_id, pinned)
    }

    fn order_session(&self, session_id: &str, before: Option<&str>) -> bool {
        self.server.ahp_inner().order_session(session_id, before)
    }

    fn compact(&self, session_id: &str, instructions: Option<String>) -> Result<(), RuntimeError> {
        self.server.ahp_inner().compact(session_id, instructions)
    }

    fn plan_seed(&self, session_id: &str, plan_file: &str) -> Result<(), RuntimeError> {
        self.server.ahp_inner().plan_seed(session_id, plan_file)
    }

    fn goal(
        &self,
        session_id: &str,
        action: &str,
        objective: Option<String>,
        budget: Option<u64>,
        max_rounds: Option<u64>,
    ) -> Result<(), RuntimeError> {
        self.server
            .ahp_inner()
            .goal(session_id, action, objective, budget, max_rounds)
    }

    fn journal_feed(&self, session_id: &str) -> Option<manox_agent::thread::ThreadHandle> {
        self.server.ahp_inner().session_thread(session_id)
    }

    fn set_embedder_tools(
        &self,
        session_id: &str,
        client_id: &str,
        tools: Vec<manox_ahp_runtime::runtime_trait::ClientToolSpec>,
    ) {
        self.server
            .ahp_inner()
            .set_embedder_tools(session_id, client_id, tools);
    }

    fn confirm_tool_call(
        &self,
        session_id: &str,
        auth_id: &str,
        approved: bool,
    ) -> Result<(), RuntimeError> {
        let response = if approved {
            manox_agent::permission::ToolAuthorizationResponse::Decision(
                manox_agent::permission::PermissionDecision::AllowOnce,
            )
        } else {
            manox_agent::permission::ToolAuthorizationResponse::Decision(
                manox_agent::permission::PermissionDecision::Deny,
            )
        };
        match self.server.ahp_inner().session_thread(session_id) {
            Some(thread) => {
                thread.with_mut(|t| t.respond_authorization(auth_id, response));
                Ok(())
            }
            None => Err(RuntimeError::new(format!("unknown session {session_id}"))
                .with_code(manox_ahp_runtime::error::codes::SESSION_NOT_FOUND)),
        }
    }

    fn answer_question(&self, session_id: &str, request_id: &str) -> Result<(), RuntimeError> {
        match self.server.ahp_inner().session_thread(session_id) {
            Some(thread) => {
                thread.with_mut(|t| {
                    t.respond_question(
                        request_id,
                        manox_agent::questions::AskOutcome::Answered(Vec::new()),
                    )
                });
                Ok(())
            }
            None => Err(RuntimeError::new(format!("unknown session {session_id}"))
                .with_code(manox_ahp_runtime::error::codes::SESSION_NOT_FOUND)),
        }
    }
}
