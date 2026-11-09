//! The gateway's implementation of the AHP runtime seam.
//!
//! [`SessionRuntime`] is what the AHP adapter speaks; this module is where those
//! 22 methods land on the live session gateway. The adapter itself never names a
//! gateway type — it holds a `dyn SessionRuntime` — so the two are coupled at
//! exactly this file.
//!
//! **Every method here is a delegation, not a reimplementation.** The gateway's
//! `AgentServerInner` intents are the single write path; this adapter exists to
//! give them a name the AHP side can hold without depending on the gateway's
//! crate layout.

use std::sync::Arc;

use async_trait::async_trait;
use manox_ahp_runtime::error::RuntimeError;
use manox_ahp_runtime::runtime_trait::{
    ForkIntent, RenameOutcome, SessionIntent, SessionRuntime, TerminalSnapshot,
};
use serde_json::Value;

use crate::agent_server::AgentServer;

/// The gateway as the AHP runtime.
pub struct GatewayRuntime {
    server: Arc<AgentServer>,
    metrics_cache: Arc<std::sync::Mutex<manox_ahp_runtime::journal_query::ConversationInfoCache>>,
}

impl GatewayRuntime {
    /// Wrap a live server.
    pub fn new(server: Arc<AgentServer>) -> Self {
        Self {
            server,
            metrics_cache: Default::default(),
        }
    }

    /// The wrapped server, for the gateway's own wiring (the `/ahp` route needs
    /// to reach it).
    pub fn server(&self) -> &Arc<AgentServer> {
        &self.server
    }
}

#[async_trait]
impl SessionRuntime for GatewayRuntime {
    fn terminal_state(&self, terminal_id: &str) -> Option<TerminalSnapshot> {
        {
            let _ = terminal_id;
            None
        }
    }

    fn terminal_infos(&self) -> Vec<ahp_types::state::TerminalInfo> {
        Vec::new()
    }

    fn terminal_raw_tap(
        &self,
        terminal_id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<std::sync::Arc<Vec<u8>>>> {
        {
            let _ = terminal_id;
            None
        }
    }

    async fn create_terminal(
        &self,
        session_id: &str,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), RuntimeError> {
        {
            let _ = (session_id, terminal_id, cols, rows);
            Err(RuntimeError::new(
                "terminal support is not built into this host",
            ))
        }
    }

    async fn dispose_terminal(&self, terminal_id: &str) -> Result<(), RuntimeError> {
        {
            let _ = terminal_id;
            Err(RuntimeError::new(
                "terminal support is not built into this host",
            ))
        }
    }

    async fn terminal_input(&self, terminal_id: &str, data: &str) -> Result<(), RuntimeError> {
        {
            let _ = (terminal_id, data);
            Err(RuntimeError::new(
                "terminal support is not built into this host",
            ))
        }
    }

    async fn terminal_resize(
        &self,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), RuntimeError> {
        {
            let _ = (terminal_id, cols, rows);
            Err(RuntimeError::new(
                "terminal support is not built into this host",
            ))
        }
    }

    async fn create_session(&self, owner: &str, intent: SessionIntent) -> Result<(), RuntimeError> {
        use crate::agent_server::AgentServerInner;
        let server = Arc::clone(&self.server);
        let inner = Arc::clone(server.ahp_inner());
        let owner = owner.to_string();
        let _ = AgentServerInner::create_session_request(&inner, &owner, intent).await?;
        Ok(())
    }

    async fn fork_session(&self, owner: &str, intent: ForkIntent) -> Result<(), RuntimeError> {
        let server = Arc::clone(&self.server);
        let inner = Arc::clone(server.ahp_inner());
        let owner = owner.to_string();
        let _ = crate::agent_server::fork_session(&inner, &owner, intent).await?;
        Ok(())
    }

    async fn dispose_session(&self, owner: &str, session_id: &str) -> Result<(), RuntimeError> {
        self.server.ahp_inner().dispose_session(owner, session_id);
        Ok(())
    }

    fn has_session(&self, session_id: &str) -> bool {
        self.server.ahp_inner().session_thread(session_id).is_some()
    }

    async fn open_session(&self, owner: &str, session_id: &str) -> Result<(), RuntimeError> {
        let inner = Arc::clone(self.server.ahp_inner());
        let owner = owner.to_string();
        let id = session_id.to_string();
        crate::agent_server::open_session(&inner, &owner, &id).await
    }

    async fn submit(
        &self,
        owner: &str,
        session_id: &str,
        text: String,
    ) -> Result<Value, RuntimeError> {
        let server = Arc::clone(&self.server);
        let inner = Arc::clone(server.ahp_inner());
        let owner = owner.to_string();
        let session_id = session_id.to_string();
        inner
            .submit(&owner, &session_id, text, Vec::new(), None, None)
            .await
    }

    async fn steer(
        &self,
        session_id: &str,
        message_id: &str,
        text: String,
    ) -> Result<Value, RuntimeError> {
        self.server
            .ahp_inner()
            .steer(session_id, message_id.to_string(), text, Vec::new(), None)
    }

    fn drop_queued(&self, session_id: &str, message_id: &str) {
        self.server
            .ahp_inner()
            .drop_queued(session_id, message_id.to_string());
    }

    async fn cancel_turn(&self, session_id: &str) -> Result<(), RuntimeError> {
        tracing::info!(session = %session_id, "gateway: cancelling the turn");
        // A cold session (restored journal, no live engine in THIS process)
        // still owes the cancel an answer: open it first, so the cancel
        // either lands on its just-loaded thread (a no-op settle — the
        // engine that owed the turn died elsewhere) or reports the foreign
        // write lease honestly. Refusing "unknown session" left a restored
        // dead turn unstoppable (real-device round, 2026-10-20).
        self.ensure_live("ahp", session_id).await?;
        match self.server.ahp_inner().session_thread(session_id) {
            Some(thread) => {
                thread.with_mut(|t| t.cancel());
                Ok(())
            }
            None => Err(RuntimeError::not_found(format!(
                "unknown session {session_id}"
            ))),
        }
    }

    async fn set_model(&self, session_id: &str, model: &str) -> Result<(), RuntimeError> {
        self.ensure_live("ahp", session_id).await?;
        self.server.ahp_inner().set_model(session_id, model)
    }

    async fn preflight_config(
        &self,
        session_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
        mode: Option<&str>,
        network: Option<&str>,
    ) -> Result<(), RuntimeError> {
        self.ensure_live("ahp", session_id).await?;
        self.server
            .ahp_inner()
            .preflight_config(session_id, model, effort, mode, network)
    }

    async fn set_reasoning_effort(
        &self,
        session_id: &str,
        effort: &str,
    ) -> Result<(), RuntimeError> {
        self.ensure_live("ahp", session_id).await?;
        self.server
            .ahp_inner()
            .set_reasoning_effort(session_id, effort)
    }

    async fn set_access_mode(&self, session_id: &str, mode: &str) -> Result<(), RuntimeError> {
        self.ensure_live("ahp", session_id).await?;
        self.server.ahp_inner().set_access_mode(session_id, mode)
    }

    async fn set_network_access(&self, session_id: &str, access: &str) -> Result<(), RuntimeError> {
        self.ensure_live("ahp", session_id).await?;
        self.server
            .ahp_inner()
            .set_network_access(session_id, access)
    }

    async fn set_cwd(&self, session_id: &str, cwd: &str) -> Result<(), RuntimeError> {
        self.ensure_live("ahp", session_id).await?;
        let server = Arc::clone(&self.server);
        let inner = Arc::clone(server.ahp_inner());
        let session_id = session_id.to_string();
        let cwd = cwd.to_string();
        inner.set_cwd(&session_id, &cwd).await
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

    fn set_annotation(&self, session_id: &str, annotation: &serde_json::Value) -> bool {
        self.server
            .ahp_inner()
            .set_annotation(session_id, annotation)
    }

    fn remove_annotation(&self, session_id: &str, annotation_id: &str) -> bool {
        self.server
            .ahp_inner()
            .remove_annotation(session_id, annotation_id)
    }

    fn pin_session(&self, session_id: &str, pinned: bool) -> bool {
        self.server.ahp_inner().pin_session(session_id, pinned)
    }

    fn order_session(&self, session_id: &str, before: Option<&str>) -> bool {
        self.server.ahp_inner().order_session(session_id, before)
    }

    fn set_project_alias(&self, path: &str, alias: Option<&str>) -> bool {
        self.server.ahp_inner().set_project_alias(path, alias)
    }

    async fn compact(
        &self,
        session_id: &str,
        instructions: Option<String>,
    ) -> Result<(), RuntimeError> {
        self.ensure_live("ahp", session_id).await?;
        self.server.ahp_inner().compact(session_id, instructions)
    }

    async fn goal(
        &self,
        session_id: &str,
        action: &str,
        objective: Option<String>,
        budget: Option<u64>,
        max_rounds: Option<u64>,
    ) -> Result<(), RuntimeError> {
        self.ensure_live("ahp", session_id).await?;
        self.server
            .ahp_inner()
            .goal(session_id, action, objective, budget, max_rounds)
    }

    fn journal_feed(&self, session_id: &str) -> Option<manox_agent::thread::ThreadHandle> {
        self.server.ahp_inner().session_thread(session_id)
    }

    async fn teardown_container(
        &self,
        owner: &str,
        session_id: &str,
        mode: &str,
    ) -> Result<Value, RuntimeError> {
        self.server
            .ahp_inner()
            .teardown_container(owner, session_id, mode)
            .await
    }

    async fn conversation_metrics(&self, session_id: &str) -> Option<Value> {
        let thread = self.server.ahp_inner().session_thread(session_id)?;
        let cache = Arc::clone(&self.metrics_cache);
        manox_ahp_runtime::journal_query::conversation_info(&cache, &thread, session_id)
            .await
            .ok()
    }

    async fn set_embedder_tools(
        &self,
        session_id: &str,
        client_id: &str,
        tools: Vec<manox_ahp_runtime::runtime_trait::ClientToolSpec>,
    ) {
        self.server
            .ahp_inner()
            .set_embedder_tools(session_id, client_id, tools);
    }

    async fn confirm_tool_call(
        &self,
        session_id: &str,
        auth_id: &str,
        approved: bool,
        selected_option_id: Option<String>,
    ) -> Result<(), RuntimeError> {
        // The clicked option refines an approve verdict into its amendment
        // shapes (the network host cards); an absent id (the bare allow/deny
        // pair) keeps the plain verdict.
        let decision = match (approved, selected_option_id.as_deref()) {
            (true, Some("allow_host_session")) => {
                manox_agent::permission::PermissionDecision::AllowHostSession
            }
            (true, Some("allow_host_persistent")) => {
                manox_agent::permission::PermissionDecision::AllowHostPersistent
            }
            (true, _) => manox_agent::permission::PermissionDecision::AllowOnce,
            (false, _) => manox_agent::permission::PermissionDecision::Deny,
        };
        let response = manox_agent::permission::ToolAuthorizationResponse::Decision(decision);
        // §D.6 settle path: a subscribed-but-never-submitted session has
        // served state without a live engine in the sessions table, so the
        // bare lookup would refuse a verdict as `unknown session` — the
        // replayed park the restore re-parks would stay unanswerable
        // forever. Cold-open the thread first; its restore re-arms the
        // gates the chain still carries.
        let inner = Arc::clone(self.server.ahp_inner());
        let thread = crate::agent_server::ensure_opened_for_settle(&inner, session_id).await?;
        thread.with_mut(|t| t.respond_authorization(auth_id, response));
        Ok(())
    }

    async fn answer_question(
        &self,
        session_id: &str,
        request_id: &str,
        response: manox_ahp_runtime::runtime_trait::QuestionResponse,
        answers: Vec<manox_agent::permission::AskAnswer>,
    ) -> Result<(), RuntimeError> {
        tracing::debug!(
            session = %session_id,
            request_id = %request_id,
            ?response,
            answers = answers.len(),
            "gateway: answering question"
        );
        // Same settle path as `confirm_tool_call`: cold-open before the
        // bare lookup can refuse the verdict.
        let inner = Arc::clone(self.server.ahp_inner());
        let thread = crate::agent_server::ensure_opened_for_settle(&inner, session_id).await?;
        use manox_ahp_runtime::runtime_trait::QuestionResponse;
        let outcome = match response {
            QuestionResponse::Answer => manox_agent::questions::AskOutcome::Answered(answers),
            QuestionResponse::Dismiss => manox_agent::questions::AskOutcome::Dismissed,
            QuestionResponse::Cancel => manox_agent::questions::AskOutcome::Cancelled,
        };
        thread.with_mut(|t| t.respond_question(request_id, outcome));
        Ok(())
    }

    // MCP servers are process-global; the session id rides the trait for
    // symmetry with every other seam but carries no per-session scoping.
    // Both calls are detachable intents that await nothing: the registry
    // settles the lifecycle detached and the AHP face learns the outcome
    // from the event stream.

    async fn mcp_start(&self, _session_id: &str, id: &str) -> Result<(), RuntimeError> {
        #[cfg(feature = "mcp")]
        {
            manox_agent::mcp::start(id).map_err(RuntimeError::new)
        }
        #[cfg(not(feature = "mcp"))]
        {
            let _ = id;
            Err(RuntimeError::new("MCP support is not built into this host"))
        }
    }

    async fn mcp_stop(&self, _session_id: &str, id: &str) -> Result<(), RuntimeError> {
        #[cfg(feature = "mcp")]
        {
            manox_agent::mcp::stop(id).map_err(RuntimeError::new)
        }
        #[cfg(not(feature = "mcp"))]
        {
            let _ = id;
            Err(RuntimeError::new("MCP support is not built into this host"))
        }
    }

    async fn mcp_set_enabled(
        &self,
        _session_id: &str,
        id: &str,
        enabled: bool,
    ) -> Result<(), RuntimeError> {
        #[cfg(feature = "mcp")]
        {
            // Validate the id against the MCP face *before* touching the
            // settings file: a refusal must not leave a phantom name in
            // `[mcp] disabled` that would silently disable a future server.
            // A real server whose live start then fails keeps its decision —
            // a failed start is a visible Error slot the client can retry,
            // not a silently reverted toggle.
            let known = manox_agent::mcp::try_global()
                .is_some_and(|registry| registry.slot(id).is_some())
                || manox_agent::mcp::load_merged_config()
                    .mcp_servers
                    .contains_key(id);
            if !known {
                return Err(RuntimeError::new(format!("unknown MCP server: {id}")));
            }
            manox_agent::settings::modify_mcp_disabled(|mut disabled| {
                if enabled {
                    disabled.retain(|name| name != id);
                } else if !disabled.iter().any(|name| name == id) {
                    disabled.push(id.to_string());
                }
                disabled
            })
            .map_err(|e| RuntimeError::new(format!("persisting MCP enablement: {e:#}")))?;
            if enabled {
                manox_agent::mcp::start(id).map_err(RuntimeError::new)
            } else {
                manox_agent::mcp::stop(id).map_err(RuntimeError::new)
            }
        }
        #[cfg(not(feature = "mcp"))]
        {
            let _ = (id, enabled);
            Err(RuntimeError::new("MCP support is not built into this host"))
        }
    }
}
