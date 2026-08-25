//! Permission gating for the pi harness (host-layer policy).
//!
//! The pi kernel exposes the `requires_approval` seam on `AgentTool` but
//! deliberately ships no gate — permission policy is a harness concern. This
//! module is the pi path's gate: pure mode-based allow/deny, fully
//! synchronous, no reviewer and no interactive approval round trip.
//!
//! - `DangerFullAccess` runs every gated call ungated (bash unsandboxed).
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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pi::tool::{
    AgentTool as PiAgentTool, AgentToolResult, ExecutionMode, ToolContext, ToolError, ToolProgress,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::permission::{PendingAuthMeta, PermissionDecision, ToolAuthorizationResponse};
use crate::thread::{PermissionMode, ThreadEvent, ToolCallStatus};
use crate::thread_engine::BackendNotice;

/// Model-facing denial marker (always English, never i18n): read-only mode
/// refuses every fs mutation. Matches deepseek's `[sandbox: …]` vocabulary.
const DENY_READ_ONLY: &str = "[sandbox: file access denied under read-only mode]";
/// Model-facing denial marker: workspace-write refuses a mutation whose
/// target the policy cannot prove inside the writable roots.
const DENY_OUT_OF_WORKSPACE: &str = "[sandbox: file access denied under workspace-write mode]";

/// The same-turn escalation hint appended to a fs-mutation refusal.
fn fs_escalation_hint() -> String {
    pi_extensions::sandbox::escalation_hint_marker("operation")
}

/// A refused fs mutation: the marker for `mode` plus the escalation hint.
fn fs_denial(marker: &str) -> ToolError {
    ToolError::Other(format!("{marker}\n{}", fs_escalation_hint()))
}

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

/// Host `EscalationApprover` over the `ApprovalGate`: parks a
/// `sandbox_permissions` escalation on the user via the same
/// `ToolCallAuthorization` round-trip `AskUserQuestion` uses, mapping the
/// decision to the closed escalation outcome vocabulary.
pub struct GateEscalationApprover {
    gate: Arc<ApprovalGate>,
}

impl GateEscalationApprover {
    pub fn new(gate: Arc<ApprovalGate>) -> Self {
        Self { gate }
    }
}

#[async_trait::async_trait]
impl pi_extensions::sandbox::EscalationApprover for GateEscalationApprover {
    async fn request(
        &self,
        req: pi_extensions::sandbox::EscalationRequest,
    ) -> pi_extensions::sandbox::EscalationOutcome {
        let mode = req.requested_mode;
        let tool_name = req.tool_name.clone();
        let reason = format!("escalate sandbox to {}: {}", mode.wire(), req.justification);
        let input = serde_json::json!({
            "sandbox_permissions": mode.wire(),
            "justification": req.justification,
        });
        let rx = self.gate.register(
            &req.call_id,
            PendingAuthMeta {
                tool_name: tool_name.clone(),
                summary: reason.clone(),
                input: input.clone(),
            },
        );
        self.gate.emit(ThreadEvent::ToolCallAuthorization {
            id: req.call_id.clone(),
            tool_name,
            summary: reason,
            input,
        });
        let response = match req.signal {
            Some(signal) => tokio::select! {
                r = rx => r.unwrap_or(ToolAuthorizationResponse::Decision(PermissionDecision::Deny)),
                _ = signal.cancelled() => {
                    self.gate.discard(&req.call_id);
                    return pi_extensions::sandbox::EscalationOutcome::Cancelled;
                }
            },
            None => rx.await.unwrap_or(ToolAuthorizationResponse::Decision(
                PermissionDecision::Deny,
            )),
        };
        self.gate.discard(&req.call_id);
        match response {
            ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce) => {
                pi_extensions::sandbox::EscalationOutcome::AllowedOnce
            }
            _ => pi_extensions::sandbox::EscalationOutcome::Rejected,
        }
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
    /// Optional sandbox-escalation config (Write/Edit): resolves a
    /// `sandbox_permissions` grant through the host approver before the
    /// mode check, so an approved wider mode widens the verdict for one
    /// call. The grant is a per-call local value (no shared cell —
    /// Write/Edit run `Parallel`, a shared cell would race). Bash resolves
    /// its own escalation inside the tool.
    escalation_approver: Option<Arc<dyn pi_extensions::sandbox::EscalationApprover + Send + Sync>>,
    mode_resolver: Option<Arc<dyn Fn() -> PermissionMode + Send + Sync>>,
    /// Live extra writable roots granted by an approved `EnterWorktree`
    /// (the worktree, its git common dir, the pre-enter project root) —
    /// resolved per call so a swap between calls is picked up. Absent when
    /// the session never entered a worktree.
    worktree_roots: Option<Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>>,
}

pub type AutoAllowResolver = Arc<dyn Fn(&str, &serde_json::Value) -> bool + Send + Sync>;

impl ApprovalGatedTool {
    pub fn new(inner: Arc<dyn PiAgentTool>, gate: Arc<ApprovalGate>) -> Self {
        Self {
            inner,
            gate,
            plan_policy: None,
            auto_allow: None,
            escalation_approver: None,
            mode_resolver: None,
            worktree_roots: None,
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

    /// Attach the sandbox-escalation config (Write/Edit): the host approver +
    /// the standing-mode resolver. When the model passes
    /// `sandbox_permissions`+`justification`, the gate resolves the wider
    /// mode through the approver and the returned per-call grant widens the
    /// verdict for that one call (no shared cell — Write/Edit are parallel).
    pub fn with_escalation(
        mut self,
        approver: Arc<dyn pi_extensions::sandbox::EscalationApprover + Send + Sync>,
        mode_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
    ) -> Self {
        self.escalation_approver = Some(approver);
        self.mode_resolver = Some(mode_resolver);
        self
    }

    /// Attach the live worktree-granted-roots resolver (Write/Edit): the
    /// verdict admits targets under the roots an approved `EnterWorktree`
    /// granted, mirroring the seatbelt's additive roots.
    pub fn with_worktree_roots(
        mut self,
        resolver: Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>,
    ) -> Self {
        self.worktree_roots = Some(resolver);
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
        // Resolve a one-shot sandbox-escalation grant (Write/Edit) before the
        // mode check; an approved wider mode widens the verdict for this call
        // only — a per-call local value, not a shared cell (Write/Edit run
        // `Parallel`; a shared cell would race between concurrent calls).
        // Bash resolves its own escalation inside the tool and never reaches
        // here (auto-allowed).
        let standing = self.standing_mode();
        let grant = self
            .resolve_escalation(tool_call_id, &signal, &params, standing)
            .await?;
        let mode = grant.unwrap_or(standing);
        match mode {
            PermissionMode::DangerFullAccess => {
                self.delegate(tool_call_id, params, signal, ctx, progress)
                    .await
            }
            PermissionMode::WorkspaceWrite => {
                self.workspace_write_verdict(&params, ctx)?;
                self.delegate(tool_call_id, params, signal, ctx, progress)
                    .await
            }
            PermissionMode::ReadOnly => Err(fs_denial(DENY_READ_ONLY)),
        }
    }

    /// The standing session mode (absent any per-call grant).
    fn standing_mode(&self) -> PermissionMode {
        self.mode_resolver
            .as_ref()
            .map(|r| r())
            .unwrap_or_else(|| self.gate.mode())
    }

    /// Validate the `sandbox_permissions`+`justification` pairing and, when
    /// present, resolve the wider mode through the host approver, returning
    /// it as a per-call grant (no shared state — deepseek parity). `None`
    /// means no escalation was requested. A non-widening or unapproved
    /// request returns the verbatim error (fail-closed).
    async fn resolve_escalation(
        &self,
        tool_call_id: &str,
        signal: &CancellationToken,
        params: &serde_json::Value,
        standing: PermissionMode,
    ) -> Result<Option<PermissionMode>, ToolError> {
        let sp = params.get("sandbox_permissions").and_then(|v| v.as_str());
        let just = params.get("justification").and_then(|v| v.as_str());
        pi_extensions::sandbox::validate_escalation_args(sp, just)
            .map_err(ToolError::InvalidArguments)?;
        let Some(requested) = sp else {
            return Ok(None);
        };
        let approver = self.escalation_approver.as_ref().ok_or_else(|| {
            ToolError::Other(
                "sandbox escalation requires approval, but no approval service is composed".into(),
            )
        })?;
        let requested =
            pi_extensions::sandbox::PermissionMode::from_wire(requested).ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "sandbox_permissions must be one of: {}",
                    pi_extensions::sandbox::ESCALATION_TARGETS
                        .iter()
                        .map(|m| m.wire())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
        let grant = pi_extensions::sandbox::approve_escalation(
            pi_extensions::sandbox::EscalationRequest {
                requested_mode: requested,
                justification: just.unwrap().to_string(),
                effective_mode: standing,
                subject: "operation".into(),
                tool_name: self.inner.name().to_string(),
                call_id: tool_call_id.to_string(),
                signal: Some(signal.clone()),
            },
            Some(approver.as_ref()),
        )
        .await
        .map_err(ToolError::Other)?;
        Ok(Some(grant))
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
        let deny = || Err(fs_denial(DENY_OUT_OF_WORKSPACE));
        // Containment against the shared writable-root set (workspace + manox
        // home + /tmp + tmpdir) plus the worktree-granted roots an approved
        // `EnterWorktree` added — no `.git` or plans-dir special-casing: the
        // plans dir is admitted transitively under the manox home.
        let mut roots = pi_extensions::sandbox::writable_roots(PermissionMode::WorkspaceWrite, cwd);
        if let Some(resolver) = &self.worktree_roots {
            roots.extend(resolver());
        }
        let contained = |target: &Path| {
            let canon = pi_extensions::sandbox::canonicalize_best_effort(target);
            roots.iter().any(|r| canon.starts_with(r))
        };
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
                if !contained(&target) {
                    return deny();
                }
                Ok(())
            }
            "Edit" => {
                let Some(patch) = params.get("patch").and_then(|v| v.as_str()) else {
                    return deny();
                };
                let file_patches = match pi::hashline::parse_patch(patch) {
                    Ok(p) => p,
                    // Unverifiable targets fail closed (the Edit tool
                    // rejects malformed hashline anyway).
                    Err(_) => return deny(),
                };
                for fp in &file_patches {
                    let target = if fp.path.is_absolute() {
                        fp.path.clone()
                    } else {
                        cwd.join(&fp.path)
                    };
                    if !contained(&target) {
                        return deny();
                    }
                }
                Ok(())
            }
            // Session/repo-scoped coordination carries no out-of-workspace
            // write target: team orchestration, the shared task list, and
            // worktree enter/exit run under WorkspaceWrite; ReadOnly still
            // denies them at the mode match, DangerFullAccess never consults this.
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
        let mut schema = self.inner.parameters_schema();
        // Advertise the escalation fields on the wrapping tool (Write/Edit)
        // when a sandbox backend is mounted, so the model knows the denied
        // call can be retried with `sandbox_permissions`+`justification`.
        if self.escalation_approver.is_some()
            && let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut())
        {
            props.insert(
                "sandbox_permissions".into(),
                serde_json::json!({
                    "type": "string",
                    "enum": pi_extensions::sandbox::ESCALATION_TARGETS
                        .iter()
                        .map(|m| m.wire())
                        .collect::<Vec<_>>(),
                    "description": "The wider sandbox mode this file operation needs. Only valid as a one-shot retry of an operation the sandbox just denied; requires justification and user approval."
                }),
            );
            props.insert(
                "justification".into(),
                serde_json::json!({
                    "type": "string",
                    "description": "Required with sandbox_permissions: one sentence for the user explaining why this exact file operation needs the wider access."
                }),
            );
        }
        schema
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
        gate.set_mode(PermissionMode::DangerFullAccess);
        assert_eq!(gate.mode(), PermissionMode::DangerFullAccess);
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
        gate.set_mode(PermissionMode::DangerFullAccess);
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
        assert!(err.to_string().contains(DENY_READ_ONLY));
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

    /// An approved `EnterWorktree` widens the fs fence with the granted
    /// roots: while the session cwd is the worktree, a Write to the
    /// pre-enter project root passes the verdict (orchestration keeps
    /// writing the main checkout). The target sits outside every default
    /// writable root so only the resolver can admit it.
    #[tokio::test]
    async fn workspace_write_allows_entered_worktree_roots() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let granted = PathBuf::from("/usr/local/manox-wt-fence-proj");
        let target = granted.join("probe.txt");
        let (gate, _rx) = gate_with_events();
        gate.set_mode(PermissionMode::WorkspaceWrite);

        // With the worktree resolver: the granted root admits the target.
        let ran = Arc::new(AtomicUsize::new(0));
        let tool = ApprovalGatedTool::new(
            Arc::new(MockTool {
                name: "Write",
                approval: false,
                read_only: false,
                ran: Arc::clone(&ran),
            }),
            Arc::clone(&gate),
        )
        .with_worktree_roots({
            let granted = granted.clone();
            Arc::new(move || vec![granted.clone()])
        });
        let ctx = LocalToolContext::new(
            Arc::new(TokioExecutionEnv::new(std::env::temp_dir())),
            wt.clone(),
            Arc::new(ToolState::new()),
        );
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

        // Control: the same call without the resolver is denied (the
        // worktree cwd's default roots never cover the granted path).
        let (tool2, ran2) = gated_named("Write", false, false, Arc::clone(&gate));
        let err = tool2
            .execute(
                "c2",
                serde_json::json!({"path": target, "content": "x"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains(DENY_OUT_OF_WORKSPACE));
        assert_eq!(ran2.load(Ordering::SeqCst), 0);
    }

    /// The manox state home is part of the workspace-write writable scope:
    /// plan files (and other session state) write without escalation. The
    /// target resolves through the HOST home resolver while the writable
    /// roots resolve through the extension layer's — this test pins that the
    /// two agree.
    #[tokio::test]
    async fn workspace_write_allows_manox_home_write() {
        let Ok(home) = crate::paths::manox_home() else {
            return; // no home dir to admit
        };
        let target = home.join("plans/gate-probe-plan.md");
        let (gate, _rx) = gate_with_events();
        gate.set_mode(PermissionMode::WorkspaceWrite);
        let ctx = tool_ctx();

        let (tool, ran) = gated_named("Write", false, false, Arc::clone(&gate));
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

        let (tool, ran) = gated_named("Edit", false, false, Arc::clone(&gate));
        let patch = format!(
            "*** Begin Patch\n[{}#1A2B3C]\nDEL 1\n*** End Patch",
            target.display()
        );
        let result = tool
            .execute(
                "c2",
                serde_json::json!({"patch": patch}),
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
        assert!(err.to_string().contains(DENY_OUT_OF_WORKSPACE));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(rx.try_recv().is_err(), "no authorization event on deny");
    }

    #[tokio::test]
    async fn workspace_write_edit_patch_confines_targets() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(PermissionMode::WorkspaceWrite);
        let (tool, ran) = gated_named("Edit", false, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let ok_patch = "*** Begin Patch\n[src/ok.rs#1A2B3C]\nDEL 1\n*** End Patch";
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
            "*** Begin Patch\n[/etc/manox-gate-test/bad.rs#1A2B3C]\nDEL 1\n*** End Patch";
        let err = tool
            .execute(
                "c2",
                serde_json::json!({"patch": escape_patch}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains(DENY_OUT_OF_WORKSPACE));
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
        assert!(err.to_string().contains(DENY_OUT_OF_WORKSPACE));
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
            assert!(
                err.to_string().contains(DENY_READ_ONLY),
                "{name} denied in ReadOnly"
            );
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

    struct CannedEscalationApprover(pi_extensions::sandbox::EscalationOutcome);

    #[async_trait::async_trait]
    impl pi_extensions::sandbox::EscalationApprover for CannedEscalationApprover {
        async fn request(
            &self,
            _req: pi_extensions::sandbox::EscalationRequest,
        ) -> pi_extensions::sandbox::EscalationOutcome {
            self.0
        }
    }

    /// Regression: an approved escalation whose verdict then rejects (the
    /// typical case — the model asked for an out-of-workspace target) must
    /// clear the per-call grant so the NEXT call (no escalation) falls back
    /// to the standing read-only mode, not the stale workspace-write grant.
    #[tokio::test]
    async fn read_only_escalation_rejected_by_verdict_does_not_leak_grant() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(PermissionMode::ReadOnly);
        let (tool, _ran) = gated_named("Write", false, false, Arc::clone(&gate));
        let standing = Arc::clone(&gate);
        let mode_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync> =
            Arc::new(move || standing.mode());
        let tool = tool.with_escalation(
            Arc::new(CannedEscalationApprover(
                pi_extensions::sandbox::EscalationOutcome::AllowedOnce,
            )),
            mode_resolver,
        );
        let ctx = tool_ctx();
        // Call 1: escalate to workspace-write for an out-of-workspace target.
        let args = serde_json::json!({
            "path": "/etc/manox-grant-leak-test/x.txt",
            "content": "x",
            "sandbox_permissions": "workspace-write",
            "justification": "need it"
        });
        let err = tool
            .execute("c1", args, CancellationToken::new(), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains(DENY_OUT_OF_WORKSPACE),
            "call 1 rejected by the verdict: {err}"
        );
        // Call 2: no escalation, same out-of-workspace target — must fall
        // back to read-only, NOT the stale workspace-write grant.
        let args2 = serde_json::json!({
            "path": "/etc/manox-grant-leak-test/x.txt",
            "content": "x"
        });
        let err2 = tool
            .execute("c2", args2, CancellationToken::new(), &ctx)
            .await
            .unwrap_err();
        assert!(
            err2.to_string().contains(DENY_READ_ONLY),
            "call 2 fell back to read-only: {err2}"
        );
        assert!(
            !err2.to_string().contains(DENY_OUT_OF_WORKSPACE),
            "stale grant leaked to call 2: {err2}"
        );
    }

    /// Parallel regression: an escalated Write/Edit call must not leak its
    /// grant to a concurrent non-escalated call (Write/Edit run `Parallel`).
    /// The grant is a per-call local value, so the concurrent plain call
    /// stays read-only even while the escalated one is in flight.
    #[tokio::test]
    async fn parallel_non_escalated_call_is_unaffected_by_in_flight_grant() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(PermissionMode::ReadOnly);
        let (tool, _ran) = gated_named("Write", false, false, Arc::clone(&gate));
        let standing = Arc::clone(&gate);
        let mode_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync> =
            Arc::new(move || standing.mode());
        let tool = Arc::new(tool.with_escalation(
            Arc::new(CannedEscalationApprover(
                pi_extensions::sandbox::EscalationOutcome::AllowedOnce,
            )),
            mode_resolver,
        ));
        let ctx = tool_ctx();
        let escalated = serde_json::json!({
            "path": "/etc/manox-parallel-race/x.txt",
            "content": "x",
            "sandbox_permissions": "workspace-write",
            "justification": "need it"
        });
        let plain = serde_json::json!({
            "path": "/etc/manox-parallel-race/y.txt",
            "content": "y"
        });
        let (a, b) = tokio::join!(
            tool.execute("c1", escalated, CancellationToken::new(), &ctx),
            tool.execute("c2", plain, CancellationToken::new(), &ctx),
        );
        let a = a.unwrap_err();
        let b = b.unwrap_err();
        assert!(
            a.to_string().contains(DENY_OUT_OF_WORKSPACE),
            "escalated: {a}"
        );
        // The plain call must be read-only, NOT the in-flight workspace-write
        // grant (no shared cell → no leak).
        assert!(
            b.to_string().contains(DENY_READ_ONLY),
            "plain fell back to read-only: {b}"
        );
        assert!(
            !b.to_string().contains(DENY_OUT_OF_WORKSPACE),
            "in-flight grant leaked to the parallel plain call: {b}"
        );
    }
    /// A mock tool whose schema carries a `properties` object (so the gate's
    /// escalation-field merge has somewhere to land).
    struct PropMock;
    #[async_trait::async_trait]
    impl PiAgentTool for PropMock {
        fn name(&self) -> &str {
            "Write"
        }
        fn description(&self) -> &str {
            "mock"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
        }
        fn requires_approval(&self, _params: &serde_json::Value) -> bool {
            false
        }
        fn is_read_only(&self) -> bool {
            false
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Value,
            _signal: CancellationToken,
            _ctx: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            Ok(AgentToolResult::text("ran"))
        }
    }

    #[test]
    fn escalation_fields_advertised_on_wrapped_write_edit_schema() {
        let bare_gate = gate();
        let gate = gate();
        let standing = Arc::clone(&gate);
        let mode_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync> =
            Arc::new(move || standing.mode());
        let tool = ApprovalGatedTool::new(Arc::new(PropMock), gate).with_escalation(
            Arc::new(CannedEscalationApprover(
                pi_extensions::sandbox::EscalationOutcome::AllowedOnce,
            )),
            mode_resolver,
        );
        let schema = tool.parameters_schema();
        let props = schema["properties"]
            .as_object()
            .expect("schema has properties");
        assert!(props.contains_key("path"), "inner field preserved");
        assert!(
            props.contains_key("sandbox_permissions"),
            "escalation field advertised"
        );
        assert!(
            props.contains_key("justification"),
            "escalation field advertised"
        );
        // Without escalation configured, the inner schema passes through bare.
        let bare = ApprovalGatedTool::new(Arc::new(PropMock), bare_gate);
        let bare_schema = bare.parameters_schema();
        let bare_props = bare_schema["properties"]
            .as_object()
            .expect("bare schema has properties");
        assert!(!bare_props.contains_key("sandbox_permissions"));
    }
}
