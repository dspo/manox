//! Permission gating for the pi harness (host-layer policy).
//!
//! The pi kernel exposes the `requires_approval` seam on `AgentTool` but
//! deliberately ships no gate — permission policy is a harness concern. This
//! module is the pi path's gate: pure mode-based allow/deny, fully
//! synchronous, no reviewer and no interactive approval round trip.
//!
//! - `FullAccess` runs every gated call ungated (bash unsandboxed).
//! - `WorkspaceWrite` runs a gated call when its target is provably inside
//!   the workspace (`path_policy` for `Write`/`Edit`; sandbox-confined
//!   `Bash`/`Monitor` bypass via the auto-allow resolver); anything the
//!   policy cannot classify is denied (fail-closed).
//! - `ReadOnly` denies every gated call; reads stay ungated.
//!
//! Denials return a `ToolError` to the model — the gate never parks a call
//! on the user. The only round trip left on this channel is
//! `AskUserQuestion`, which is an interaction by design, not a permission
//! prompt.

use pi::types::Model as PiModel;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pi::tool::{
    AgentTool as PiAgentTool, AgentToolResult, ExecutionMode, ToolContext, ToolError, ToolProgress,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::permission::{PendingAuthMeta, PermissionDecision, ToolAuthorizationResponse};
use crate::thread::{PermissionMode, ThreadEvent, ToolCallStatus};
use crate::thread_engine::BackendNotice;

/// Model-facing denial text (always English, never i18n): read-only mode
/// rejects every mutating/remote call.
const DENY_READ_ONLY: &str = "denied: read-only mode";
/// Model-facing denial text (always English, never i18n): workspace-write
/// rejects every call whose target the policy cannot prove in-workspace.
const DENY_OUT_OF_WORKSPACE: &str = "denied: target outside workspace (mode: workspace-write)";

/// A pending interaction parked on the user's answer (`AskUserQuestion`).
struct PendingAuth {
    tx: oneshot::Sender<ToolAuthorizationResponse>,
    meta: PendingAuthMeta,
}

/// Shared permission state for one pi thread: the mode, the pending
/// interaction round trips, and the event channel back to the UI. Lives in
/// the engine state so the tool wrappers (tokio side) and the facade's
/// respond path (gpui side) see one source of truth.
pub struct ApprovalGate {
    mode: Mutex<PermissionMode>,
    pending: Mutex<HashMap<String, PendingAuth>>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    model: Arc<Mutex<Option<PiModel>>>,
}

impl ApprovalGate {
    pub fn new(
        notice_tx: mpsc::UnboundedSender<BackendNotice>,
        model_slot: Arc<Mutex<Option<PiModel>>>,
    ) -> Self {
        Self {
            mode: Mutex::new(PermissionMode::default()),
            pending: Mutex::new(HashMap::new()),
            notice_tx,
            model: model_slot,
        }
    }

    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock().unwrap()
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().unwrap() = mode;
    }

    pub fn model(&self) -> Option<PiModel> {
        self.model.lock().unwrap().clone()
    }

    /// Live handle to the owner's model slot so dispatch-time readers (e.g.
    /// subagent spawn) inherit the current model, not an assembly snapshot.
    pub fn model_slot(&self) -> Arc<Mutex<Option<PiModel>>> {
        Arc::clone(&self.model)
    }

    fn emit(&self, event: ThreadEvent) {
        let _ = self.notice_tx.send(BackendNotice::Event(Box::new(event)));
    }

    /// Park a new interaction: stores the responder, returns the receiver
    /// the tool awaits.
    fn register(
        &self,
        id: &str,
        meta: PendingAuthMeta,
    ) -> oneshot::Receiver<ToolAuthorizationResponse> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id.to_string(), PendingAuth { tx, meta });
        rx
    }

    /// Drop a pending interaction without answering (turn cancelled).
    fn discard(&self, id: &str) {
        self.pending.lock().unwrap().remove(id);
    }

    /// Deliver the user's answer. Unknown ids are ignored (already settled).
    pub fn respond(&self, id: &str, response: ToolAuthorizationResponse) {
        if let Some(pending) = self.pending.lock().unwrap().remove(id) {
            let _ = pending.tx.send(response);
        }
    }

    /// Snapshot of pending interactions for card re-surfacing.
    pub fn pending_entries(&self) -> Vec<(String, PendingAuthMeta)> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| (id.clone(), p.meta.clone()))
            .collect()
    }
}

// ── The gating wrapper ──────────────────────────────────────────────────────

/// Wraps a pi tool with the host's permission policy. Tools that neither
/// declare `requires_approval` nor mutate anything pass straight through.
pub struct ApprovalGatedTool {
    inner: Arc<dyn PiAgentTool>,
    gate: Arc<ApprovalGate>,
    /// Plan-mode exemption: plan-file writes bypass the gate while plan
    /// mode is active (the model drafts the plan incrementally).
    plan_policy: Option<Arc<crate::plan_mode::PlanGatePolicy>>,
    auto_allow: Option<AutoAllowResolver>,
}

pub type AutoAllowResolver = Arc<dyn Fn(&str, &serde_json::Value) -> bool + Send + Sync>;

impl ApprovalGatedTool {
    pub fn new(inner: Arc<dyn PiAgentTool>, gate: Arc<ApprovalGate>) -> Self {
        Self {
            inner,
            gate,
            plan_policy: None,
            auto_allow: None,
        }
    }

    /// Attach the plan-mode gate exemption (plan-file writes stay
    /// approval-free while plan mode is active).
    pub fn with_plan_policy(mut self, policy: Arc<crate::plan_mode::PlanGatePolicy>) -> Self {
        self.plan_policy = Some(policy);
        self
    }

    pub fn with_auto_allow(mut self, resolver: AutoAllowResolver) -> Self {
        self.auto_allow = Some(resolver);
        self
    }

    /// Host gate set: the kernel's declarative hint OR any mutating tool
    /// (write/edit/bash gated, reads free). Read-only tools and tools the
    /// kernel marks approval-free run ungated. Plan-file writes during plan
    /// mode are exempt so the model can draft the plan without a denial per
    /// edit.
    fn needs_gate(&self, params: &serde_json::Value) -> bool {
        if let Some(policy) = &self.plan_policy
            && policy.is_exempt(self.inner.name(), params)
        {
            return false;
        }
        if self
            .auto_allow
            .as_ref()
            .is_some_and(|allow| allow(self.inner.name(), params))
        {
            return false;
        }
        self.inner.requires_approval(params) || !self.inner.is_read_only()
    }

    async fn delegate(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        progress: Option<&dyn ToolProgress>,
    ) -> Result<AgentToolResult, ToolError> {
        match progress {
            Some(progress) => {
                self.inner
                    .execute_with_progress(tool_call_id, params, signal, ctx, progress)
                    .await
            }
            None => self.inner.execute(tool_call_id, params, signal, ctx).await,
        }
    }

    async fn run_gated(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        progress: Option<&dyn ToolProgress>,
    ) -> Result<AgentToolResult, ToolError> {
        if !self.needs_gate(&params) {
            return self
                .delegate(tool_call_id, params, signal, ctx, progress)
                .await;
        }
        match self.gate.mode() {
            PermissionMode::FullAccess => {
                self.delegate(tool_call_id, params, signal, ctx, progress)
                    .await
            }
            PermissionMode::WorkspaceWrite => {
                self.workspace_write_verdict(&params, ctx)?;
                self.delegate(tool_call_id, params, signal, ctx, progress)
                    .await
            }
            PermissionMode::ReadOnly => Err(ToolError::Other(DENY_READ_ONLY.to_string())),
        }
    }

    /// WorkspaceWrite verdict for one gated call: `Ok` when the operation
    /// target is provably inside the workspace, `Err` (model-facing reason)
    /// otherwise. Fail-closed: any gated call the policy cannot classify is
    /// denied. Sandbox-confined bash never reaches this check (its
    /// auto-allow resolver bypasses the gate entirely).
    fn workspace_write_verdict(
        &self,
        params: &serde_json::Value,
        ctx: &dyn ToolContext,
    ) -> Result<(), ToolError> {
        let cwd = ctx.cwd();
        let policy = crate::path_policy::WritePolicy::for_project(cwd);
        let deny = || Err(ToolError::Other(DENY_OUT_OF_WORKSPACE.to_string()));
        match self.inner.name() {
            "Write" => {
                let Some(path) = params.get("path").and_then(|v| v.as_str()) else {
                    return deny();
                };
                let target = if Path::new(path).is_absolute() {
                    std::path::PathBuf::from(path)
                } else {
                    cwd.join(path)
                };
                if policy.check(&target).is_err() {
                    return deny();
                }
                Ok(())
            }
            "Edit" => {
                let Some(patch) = params.get("patch").and_then(|v| v.as_str()) else {
                    return deny();
                };
                if policy.check_edit_patch(patch, cwd).is_err() {
                    return deny();
                }
                Ok(())
            }
            // Session/repo-scoped coordination carries no out-of-workspace
            // write target: team orchestration, the shared task list, and
            // worktree enter/exit run under WorkspaceWrite; ReadOnly still
            // denies them at the mode match, FullAccess never consults this.
            crate::tools::ENTER_WORKTREE
            | crate::tools::EXIT_WORKTREE
            | "TaskCreate"
            | "TaskList"
            | "TaskUpdate"
            | "TaskGet" => Ok(()),
            // Every other gated call (escalated bash, unknown mutating
            // tools) has no in-workspace proof.
            _ => deny(),
        }
    }
}

#[async_trait::async_trait]
impl PiAgentTool for ApprovalGatedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    fn requires_approval(&self, params: &serde_json::Value) -> bool {
        self.needs_gate(params)
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.inner.execution_mode()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        self.run_gated(tool_call_id, params, signal, ctx, None)
            .await
    }

    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        progress: &dyn ToolProgress,
    ) -> Result<AgentToolResult, ToolError> {
        self.run_gated(tool_call_id, params, signal, ctx, Some(progress))
            .await
    }
}

// ── AskUserQuestion (interactive, never permission-gated) ───────────────────

/// The pi harness `AskUserQuestion` tool. Schema and semantics ported from
/// the retired manox tool: the run IS the round trip — the question card
/// renders from the `ToolCallAuthorization` event and the user's answers
/// come back through `respond_tool_authorization`, short-circuited into a
/// `ToolResult` without any execution. Read-only by contract: permission
/// modes never touch it.
pub struct PiAskUserQuestionTool {
    gate: Arc<ApprovalGate>,
}

impl PiAskUserQuestionTool {
    pub fn new(gate: Arc<ApprovalGate>) -> Self {
        Self { gate }
    }
}

#[async_trait::async_trait]
impl PiAgentTool for PiAskUserQuestionTool {
    fn name(&self) -> &str {
        crate::tools::ASK_USER_QUESTION
    }

    fn description(&self) -> &str {
        "Ask the user clarifying questions when multiple valid approaches exist \
         and the answer changes what you do next. Use only for decisions that are \
         genuinely the user's to make — not for facts you can verify yourself. \
         Each call carries 1–3 questions, each with 2–3 options; mark the \
         recommended default with recommended=true when one exists. The user \
         may also type a supplemental note. Do not use this tool to ask for \
         plan approval or to confirm obvious defaults."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        ask_user_question_schema()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        if let Err(err) = validate_ask_input(&params) {
            return Err(ToolError::InvalidArguments(err));
        }
        let lang = crate::settings::load().resolve().agent;
        let title = crate::i18n::t("workspace-clarify-title").to_string();

        let rx = self.gate.register(
            tool_call_id,
            PendingAuthMeta {
                tool_name: crate::tools::ASK_USER_QUESTION.to_string(),
                summary: title.clone(),
                input: params.clone(),
            },
        );
        self.gate.emit(ThreadEvent::ToolCall {
            id: tool_call_id.to_string(),
            name: crate::tools::ASK_USER_QUESTION.to_string(),
            title: title.clone(),
            status: ToolCallStatus::PendingApproval,
            input: Some(params.clone()),
        });
        self.gate.emit(ThreadEvent::ToolCallAuthorization {
            id: tool_call_id.to_string(),
            tool_name: crate::tools::ASK_USER_QUESTION.to_string(),
            summary: title,
            input: params,
        });

        let response = tokio::select! {
            r = rx => r.unwrap_or(ToolAuthorizationResponse::Decision(PermissionDecision::Deny)),
            _ = signal.cancelled() => {
                self.gate.discard(tool_call_id);
                ToolAuthorizationResponse::Decision(PermissionDecision::Deny)
            }
        };
        self.gate.discard(tool_call_id);

        match response {
            ToolAuthorizationResponse::AskUserQuestion { answers, response } => {
                let text = crate::prompt::render(
                    crate::prompt::PromptTemplate::WrapperAskUserQuestions,
                    lang,
                    &crate::prompt::AskUserQuestionsData {
                        answers: answers
                            .into_iter()
                            .map(|(q, a)| crate::prompt::AskUserQa {
                                question: q,
                                answer: a,
                            })
                            .collect(),
                        response,
                    },
                )
                .expect("ask user questions render");
                Ok(AgentToolResult::text(text))
            }
            // Any bare decision means the question never reached the user
            // (cancel or dismissed card): surface the denial as-is.
            _ => {
                let text = crate::prompt::render_static(
                    crate::prompt::PromptTemplate::WrapperToolDenied,
                    lang,
                )
                .expect("tool denied render");
                Ok(AgentToolResult::error(text))
            }
        }
    }
}

fn validate_ask_input(input: &serde_json::Value) -> Result<(), String> {
    let questions = input
        .get("questions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "AskUserQuestion requires a `questions` array".to_string())?;
    if !(1..=3).contains(&questions.len()) {
        return Err(format!(
            "AskUserQuestion requires 1-3 questions, got {}",
            questions.len()
        ));
    }
    for (idx, question) in questions.iter().enumerate() {
        let options = question
            .get("options")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("AskUserQuestion question {} requires `options`", idx + 1))?;
        if !(2..=3).contains(&options.len()) {
            return Err(format!(
                "AskUserQuestion question {} requires 2-3 options, got {}",
                idx + 1,
                options.len()
            ));
        }
    }
    Ok(())
}

fn ask_user_question_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "description": "1–3 questions to ask the user. Each becomes one step in the question drawer.",
                "minItems": 1,
                "maxItems": 3,
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The full question text to display."
                        },
                        "header": {
                            "type": "string",
                            "description": "Short label for the question (max 12 characters)."
                        },
                        "options": {
                            "type": "array",
                            "description": "2–3 choices for the user to select from.",
                            "minItems": 2,
                            "maxItems": 3,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {
                                        "type": "string",
                                        "description": "Concise label for the choice (1–5 words)."
                                    },
                                    "description": {
                                        "type": "string",
                                        "description": "Explanation of what the choice means or implies."
                                    },
                                    "recommended": {
                                        "type": "boolean",
                                        "description": "Whether this option is the recommended default."
                                    }
                                },
                                "required": ["label", "description"]
                            }
                        },
                        "multiSelect": {
                            "type": "boolean",
                            "description": "When true, the user may select multiple options; otherwise exactly one."
                        }
                    },
                    "required": ["question", "header", "options", "multiSelect"]
                }
            }
        },
        "required": ["questions"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::env::TokioExecutionEnv;
    use pi::tool::{LocalToolContext, ToolState};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn gate() -> Arc<ApprovalGate> {
        let (tx, _rx) = mpsc::unbounded_channel();
        Arc::new(ApprovalGate::new(tx, Arc::new(Mutex::new(None))))
    }

    fn gate_with_events() -> (Arc<ApprovalGate>, mpsc::UnboundedReceiver<BackendNotice>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(ApprovalGate::new(tx, Arc::new(Mutex::new(None)))),
            rx,
        )
    }

    fn tool_ctx() -> LocalToolContext {
        LocalToolContext::new(
            Arc::new(TokioExecutionEnv::new(std::env::temp_dir())),
            std::env::temp_dir(),
            Arc::new(ToolState::new()),
        )
    }

    struct MockTool {
        name: &'static str,
        approval: bool,
        read_only: bool,
        ran: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PiAgentTool for MockTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "mock"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn requires_approval(&self, _params: &serde_json::Value) -> bool {
            self.approval
        }
        fn is_read_only(&self) -> bool {
            self.read_only
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Value,
            _signal: CancellationToken,
            _ctx: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            self.ran.fetch_add(1, Ordering::SeqCst);
            Ok(AgentToolResult::text("ran"))
        }
    }

    fn gated_named(
        name: &'static str,
        approval: bool,
        read_only: bool,
        gate: Arc<ApprovalGate>,
    ) -> (ApprovalGatedTool, Arc<AtomicUsize>) {
        let ran = Arc::new(AtomicUsize::new(0));
        let tool = ApprovalGatedTool::new(
            Arc::new(MockTool {
                name,
                approval,
                read_only,
                ran: Arc::clone(&ran),
            }),
            gate,
        );
        (tool, ran)
    }

    fn gated(
        approval: bool,
        read_only: bool,
        gate: Arc<ApprovalGate>,
    ) -> (ApprovalGatedTool, Arc<AtomicUsize>) {
        gated_named("Mock", approval, read_only, gate)
    }

    #[test]
    fn gate_register_respond_round_trip() {
        let gate = gate();
        let rx = gate.register(
            "call-1",
            PendingAuthMeta {
                tool_name: "AskUserQuestion".into(),
                summary: "q".into(),
                input: serde_json::json!({}),
            },
        );
        assert_eq!(gate.pending_entries().len(), 1);
        gate.respond(
            "call-1",
            ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce),
        );
        assert!(gate.pending_entries().is_empty());
        let mut rx = rx;
        let response = rx.try_recv().unwrap();
        assert!(matches!(
            response,
            ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce)
        ));
        // Unknown ids are silently ignored.
        gate.respond(
            "nope",
            ToolAuthorizationResponse::Decision(PermissionDecision::Deny),
        );
    }

    #[test]
    fn gate_mode_switches() {
        let gate = gate();
        assert_eq!(gate.mode(), PermissionMode::WorkspaceWrite);
        gate.set_mode(PermissionMode::FullAccess);
        assert_eq!(gate.mode(), PermissionMode::FullAccess);
    }

    #[test]
    fn validate_ask_input_envelopes_counts() {
        let ok = serde_json::json!({
            "questions": [{
                "question": "q", "header": "h", "multiSelect": false,
                "options": [
                    {"label": "a", "description": ""},
                    {"label": "b", "description": ""}
                ]
            }]
        });
        assert!(validate_ask_input(&ok).is_ok());

        let four_options = serde_json::json!({
            "questions": [{
                "question": "q", "header": "h", "multiSelect": false,
                "options": [
                    {"label": "a", "description": ""},
                    {"label": "b", "description": ""},
                    {"label": "c", "description": ""},
                    {"label": "d", "description": ""}
                ]
            }]
        });
        assert!(validate_ask_input(&four_options).is_err());

        let no_questions = serde_json::json!({});
        assert!(validate_ask_input(&no_questions).is_err());
    }

    #[test]
    fn ask_schema_declares_limits() {
        let schema = ask_user_question_schema();
        let questions = &schema["properties"]["questions"];
        assert_eq!(questions["minItems"], 1);
        assert_eq!(questions["maxItems"], 3);
        let options = &questions["items"]["properties"]["options"];
        assert_eq!(options["minItems"], 2);
        assert_eq!(options["maxItems"], 3);
    }

    #[tokio::test]
    async fn read_only_tools_pass_through_ungated() {
        let (gate, mut rx) = gate_with_events();
        gate.set_mode(PermissionMode::ReadOnly);
        let (tool, ran) = gated(false, true, Arc::clone(&gate));
        let ctx = tool_ctx();
        let result = tool
            .execute("c1", serde_json::json!({}), CancellationToken::new(), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(rx.try_recv().is_err(), "no authorization event expected");
    }

    #[tokio::test]
    async fn full_access_delegates_gated_tools_without_prompting() {
        let (gate, mut rx) = gate_with_events();
        gate.set_mode(PermissionMode::FullAccess);
        let (tool, ran) = gated(true, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let result = tool
            .execute("c1", serde_json::json!({}), CancellationToken::new(), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn read_only_mode_denies_mutating_tools_with_tool_error() {
        let (gate, mut rx) = gate_with_events();
        gate.set_mode(PermissionMode::ReadOnly);
        let (tool, ran) = gated(true, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let err = tool
            .execute("c1", serde_json::json!({}), CancellationToken::new(), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), DENY_READ_ONLY);
        assert_eq!(ran.load(Ordering::SeqCst), 0, "denied call must not run");
        assert!(rx.try_recv().is_err(), "no authorization event on deny");
    }

    #[tokio::test]
    async fn workspace_write_allows_in_workspace_write() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(PermissionMode::WorkspaceWrite);
        let (tool, ran) = gated_named("Write", false, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        // The tool ctx's cwd is the workspace root for the gate's policy.
        let target = ctx.cwd().join("src/ok.txt");
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"path": target, "content": "x"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn workspace_write_denies_out_of_workspace_write() {
        let (gate, mut rx) = gate_with_events();
        gate.set_mode(PermissionMode::WorkspaceWrite);
        let (tool, ran) = gated_named("Write", false, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let err = tool
            .execute(
                "c1",
                serde_json::json!({"path": "/etc/manox-gate-test/x.txt", "content": "x"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), DENY_OUT_OF_WORKSPACE);
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(rx.try_recv().is_err(), "no authorization event on deny");
    }

    #[tokio::test]
    async fn workspace_write_edit_patch_confines_targets() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(PermissionMode::WorkspaceWrite);
        let (tool, ran) = gated_named("Edit", false, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let ok_patch = "*** Begin Patch\n[src/ok.rs#1A2B]\nDEL 1\n*** End Patch";
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"patch": ok_patch}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);

        let escape_patch =
            "*** Begin Patch\n[/etc/manox-gate-test/bad.rs#1A2B]\nDEL 1\n*** End Patch";
        let err = tool
            .execute(
                "c2",
                serde_json::json!({"patch": escape_patch}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), DENY_OUT_OF_WORKSPACE);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// Fail-closed: a gated mutating tool the path policy cannot classify
    /// (no path target) is denied under WorkspaceWrite.
    #[tokio::test]
    async fn workspace_write_denies_unclassifiable_targets() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(PermissionMode::WorkspaceWrite);
        let (tool, ran) = gated(true, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let err = tool
            .execute("c1", serde_json::json!({}), CancellationToken::new(), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), DENY_OUT_OF_WORKSPACE);
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    /// Session/repo-scoped coordination (team orchestration, shared task
    /// list, worktree enter/exit) carries no out-of-workspace write target:
    /// WorkspaceWrite admits it, ReadOnly still denies at the mode match.
    #[tokio::test]
    async fn workspace_write_admits_session_scoped_tools() {
        for name in [
            crate::tools::ENTER_WORKTREE,
            crate::tools::EXIT_WORKTREE,
            "TaskCreate",
            "TaskList",
            "TaskUpdate",
            "TaskGet",
        ] {
            let (gate, _rx) = gate_with_events();
            gate.set_mode(PermissionMode::WorkspaceWrite);
            let (tool, ran) = gated_named(name, false, false, Arc::clone(&gate));
            let ctx = tool_ctx();
            let result = tool
                .execute("c1", serde_json::json!({}), CancellationToken::new(), &ctx)
                .await
                .unwrap();
            assert!(!result.is_error, "{name} admitted under WorkspaceWrite");
            assert_eq!(ran.load(Ordering::SeqCst), 1, "{name} delegated");

            let (ro_gate, _rx) = gate_with_events();
            ro_gate.set_mode(PermissionMode::ReadOnly);
            let (ro_tool, ro_ran) = gated_named(name, false, false, Arc::clone(&ro_gate));
            let err = ro_tool
                .execute("c1", serde_json::json!({}), CancellationToken::new(), &ctx)
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), DENY_READ_ONLY, "{name} denied in ReadOnly");
            assert_eq!(ro_ran.load(Ordering::SeqCst), 0);
        }
    }

    fn mutating_gated() -> ApprovalGatedTool {
        ApprovalGatedTool::new(
            Arc::new(MockTool {
                name: "Mutating",
                approval: false,
                read_only: false,
                ran: Arc::new(AtomicUsize::new(0)),
            }),
            gate(),
        )
    }

    #[test]
    fn mutating_tools_reach_the_gate() {
        assert!(mutating_gated().needs_gate(&serde_json::json!({})));
    }

    #[test]
    fn explicit_auto_allow_preserves_sandboxed_bash_bypass_contract() {
        let tool = mutating_gated().with_auto_allow(Arc::new(|_, params| {
            !params["unsandboxed"].as_bool().unwrap_or(false)
        }));
        assert!(!tool.needs_gate(&serde_json::json!({"unsandboxed": false})));
        assert!(tool.needs_gate(&serde_json::json!({"unsandboxed": true})));
    }
}
