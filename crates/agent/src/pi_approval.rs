//! Approval gating for the pi harness (host-layer policy).
//!
//! The pi kernel exposes the `requires_approval` seam on `AgentTool` but
//! deliberately ships no gate — approval policy is a harness concern. This
//! module is the pi path's gate, mirroring the retired manox harness's
//! semantics:
//!
//! - `Danger` runs everything without prompting.
//! - `AutoPilot` asks the safety reviewer first (fail-closed); an `Allow`
//!   verdict runs the call, an `Ask` verdict escalates to the user.
//! - Escalations and manual-mode calls surface as the AskUserQuestion-style
//!   card (`Allow once` / `Always allow` / `Deny`) over the same
//!   `ToolCallAuthorization` round trip the workspace already renders.
//! - Always-allow grants live in a per-thread [`PermissionCache`].
//!
//! The round trip is async UI interaction, so it cannot live in a kernel
//! hook (hooks block synchronously); the wrapper parks the tool's future on
//! a oneshot and the workspace resolves it through
//! `ThreadEngine::respond_tool_authorization`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pi::coding_agent::ModelRuntime;
use pi::tool::{
    AgentTool as PiAgentTool, AgentToolResult, ExecutionMode, ToolContext, ToolError, ToolProgress,
};
use pi::types::{
    AgentContext, AgentMessage, CacheRetention, ContentBlock, Model as PiModel, StreamOptions,
    Usage as PiUsage,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::approval::ReviewVerdict;
use crate::approval_review::{self, ReviewerFn, ReviewerOutput, ReviewerRequest};
use crate::language::Language;
use crate::language_model::TokenUsage;
use crate::permission::{
    PendingAuthMeta, PermissionCache, PermissionDecision, ToolAuthorizationResponse,
};
use crate::thread::{ApprovalMode, ThreadEvent, ToolCallStatus};
use crate::thread_engine::BackendNotice;

/// Hard cap for the reviewer's escalation prose (manox parity): the reviewer
/// is asked for <=200 chars but nothing enforces it, and a multi-KB reason is
/// what wedges the model into re-try loops.
const REVIEWER_REASON_CAP: usize = 500;

/// A pending authorization parked on the UI's verdict.
struct PendingAuth {
    tx: oneshot::Sender<ToolAuthorizationResponse>,
    meta: PendingAuthMeta,
}

/// Shared approval state for one pi thread: the mode, the always-allow
/// cache, the pending round trips, and the event channel back to the UI.
/// Lives in the engine state so the tool wrappers (tokio side) and the
/// facade's respond path (gpui side) see one source of truth.
pub struct ApprovalGate {
    mode: Mutex<ApprovalMode>,
    cache: PermissionCache,
    pending: Mutex<HashMap<String, PendingAuth>>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    /// Set once the actor built its runtime; reviewer side calls need it.
    runtime: Mutex<Option<ModelRuntime>>,
    /// The session model, shared with `EngineState` so `SetModel` is visible
    /// to the reviewer without a second synchronization point.
    model: Arc<Mutex<Option<PiModel>>>,
}

impl ApprovalGate {
    pub fn new(
        notice_tx: mpsc::UnboundedSender<BackendNotice>,
        model: Arc<Mutex<Option<PiModel>>>,
    ) -> Self {
        Self {
            mode: Mutex::new(ApprovalMode::default()),
            cache: PermissionCache::default(),
            pending: Mutex::new(HashMap::new()),
            notice_tx,
            runtime: Mutex::new(None),
            model,
        }
    }

    pub fn set_runtime(&self, runtime: ModelRuntime) {
        *self.runtime.lock().unwrap() = Some(runtime);
    }

    pub fn mode(&self) -> ApprovalMode {
        *self.mode.lock().unwrap()
    }

    pub fn set_mode(&self, mode: ApprovalMode) {
        *self.mode.lock().unwrap() = mode;
    }

    pub fn cache(&self) -> &PermissionCache {
        &self.cache
    }

    pub fn model(&self) -> Option<PiModel> {
        self.model.lock().unwrap().clone()
    }

    fn runtime(&self) -> Option<ModelRuntime> {
        self.runtime.lock().unwrap().clone()
    }

    fn emit(&self, event: ThreadEvent) {
        let _ = self.notice_tx.send(BackendNotice::Event(Box::new(event)));
    }

    /// Park a new authorization: stores the responder, returns the receiver
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

    /// Drop a pending authorization without answering (turn cancelled).
    fn discard(&self, id: &str) {
        self.pending.lock().unwrap().remove(id);
    }

    /// Deliver the user's verdict. Unknown ids are ignored (already settled).
    pub fn respond(&self, id: &str, response: ToolAuthorizationResponse) {
        if let Some(pending) = self.pending.lock().unwrap().remove(id) {
            let _ = pending.tx.send(response);
        }
    }

    /// Snapshot of pending authorizations for card re-surfacing.
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

/// Wraps a pi tool with the host's approval policy. Tools that neither
/// declare `requires_approval` nor mutate anything pass straight through.
pub struct ApprovalGatedTool {
    inner: Arc<dyn PiAgentTool>,
    gate: Arc<ApprovalGate>,
}

impl ApprovalGatedTool {
    pub fn new(inner: Arc<dyn PiAgentTool>, gate: Arc<ApprovalGate>) -> Self {
        Self { inner, gate }
    }

    /// Host approval policy: the kernel's declarative hint OR any mutating
    /// tool (mirrors the manox gate set — write/edit/bash prompted, reads
    /// free). Read-only tools and tools the kernel marks approval-free run
    /// ungated.
    fn needs_gate(&self, params: &serde_json::Value) -> bool {
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

    #[allow(clippy::too_many_arguments)] // gate inputs are all distinct state
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
        let name = self.inner.name().to_string();
        if self.gate.mode() == ApprovalMode::Danger {
            return self
                .delegate(tool_call_id, params, signal, ctx, progress)
                .await;
        }
        if self.gate.cache().is_always_allowed(&name) {
            return self
                .delegate(tool_call_id, params, signal, ctx, progress)
                .await;
        }

        let title = crate::pi_engine::adapt::tool_title(&name, &params);
        let lang = crate::settings::load().resolve().agent;

        // AutoPilot: the reviewer gets first say (fail-closed). With the
        // reviewer disabled in settings or no model configured, every call
        // escalates — the user stays the final authority.
        let mut escalation_reason: Option<String> = None;
        if self.gate.mode() == ApprovalMode::AutoPilot
            && crate::settings::side_calls().approval_policy().enabled
        {
            match self
                .review(&name, &title, &params, ctx.cwd(), lang, &signal)
                .await
            {
                ReviewDisposition::Allowed => {
                    return self
                        .delegate(tool_call_id, params, signal, ctx, progress)
                        .await;
                }
                ReviewDisposition::Escalate(reason) => escalation_reason = Some(reason),
            }
        }

        let verdict = self
            .escalate(
                tool_call_id,
                &name,
                &title,
                escalation_reason.as_deref(),
                lang,
                &signal,
            )
            .await;
        match verdict {
            UserVerdict::Allow | UserVerdict::AlwaysAllow => {
                if matches!(verdict, UserVerdict::AlwaysAllow) {
                    self.gate.cache().set_always_allowed(&name);
                }
                // Restore the card to the real tool call (the escalation
                // re-branded it as the question card).
                self.gate.emit(ThreadEvent::ToolCall {
                    id: tool_call_id.to_string(),
                    name: name.clone(),
                    title: title.clone(),
                    status: ToolCallStatus::Running,
                    input: Some(params.clone()),
                });
                self.delegate(tool_call_id, params, signal, ctx, progress)
                    .await
            }
            UserVerdict::Deny => {
                self.gate.emit(ThreadEvent::ToolCall {
                    id: tool_call_id.to_string(),
                    name: name.clone(),
                    title,
                    status: ToolCallStatus::Denied,
                    input: Some(params.clone()),
                });
                let text = crate::prompt::render_static(
                    crate::prompt::PromptTemplate::WrapperToolDenied,
                    lang,
                )
                .expect("tool denied render");
                Ok(AgentToolResult::error(text))
            }
        }
    }

    /// Run the safety reviewer over this single call.
    async fn review(
        &self,
        name: &str,
        title: &str,
        params: &serde_json::Value,
        cwd: &Path,
        lang: Language,
        signal: &CancellationToken,
    ) -> ReviewDisposition {
        let (Some(runtime), Some(model)) = (self.gate.runtime(), self.gate.model()) else {
            return ReviewDisposition::Escalate(
                "no model configured for the safety reviewer".to_string(),
            );
        };
        let reviewer = pi_reviewer(runtime, model);
        let outcome =
            approval_review::review(&reviewer, name, params, title, cwd, lang, signal.clone())
                .await;
        self.gate.emit(ThreadEvent::ApprovalDecision {
            tool_name: name.to_string(),
            tool_title: title.to_string(),
            verdict: outcome.verdict.clone(),
        });
        match outcome.verdict {
            ReviewVerdict::Allow => ReviewDisposition::Allowed,
            ReviewVerdict::Ask { reason } => {
                ReviewDisposition::Escalate(truncate_str(&reason, REVIEWER_REASON_CAP))
            }
        }
    }

    /// Surface the escalation card and park on the user's verdict.
    #[allow(clippy::too_many_arguments)] // card inputs are all distinct state
    async fn escalate(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        title: &str,
        reason: Option<&str>,
        lang: Language,
        signal: &CancellationToken,
    ) -> UserVerdict {
        let question = crate::prompt::render(
            crate::prompt::PromptTemplate::WrapperEscalatedApprovalQuestion,
            lang,
            &crate::prompt::EscalatedApprovalQuestionData {
                tool_title: title.to_string(),
                reason: reason.map(|r| r.to_string()).unwrap_or_else(|| {
                    crate::i18n::t("workspace-escalation-no-verdict-reason").to_string()
                }),
            },
        )
        .expect("escalated approval question render");
        // Resolve the decision labels once: they ride the payload verbatim and
        // the verdict parsing below compares against these exact strings, so a
        // UI locale swap while the card is open can neither flip nor strand
        // the verdict.
        let allow_once_label = crate::i18n::t("workspace-escalation-allow-once");
        let always_allow_label = crate::i18n::t("workspace-escalation-always-allow");
        let deny_label = crate::i18n::t("workspace-escalation-deny");
        let payload = serde_json::json!({
            "questions": [{
                "question": question,
                "header": crate::i18n::t("workspace-approval-title").to_string(),
                "multiSelect": false,
                "options": [
                    {
                        "label": allow_once_label.to_string(),
                        "description": crate::i18n::t("workspace-escalation-allow-once-desc").to_string(),
                    },
                    {
                        "label": always_allow_label.to_string(),
                        "description": crate::i18n::t("workspace-escalation-always-allow-desc").to_string(),
                    },
                    {
                        "label": deny_label.to_string(),
                        "description": crate::i18n::t("workspace-escalation-deny-desc").to_string(),
                    }
                ]
            }]
        });

        let rx = self.gate.register(
            tool_call_id,
            PendingAuthMeta {
                tool_name: tool_name.to_string(),
                summary: title.to_string(),
                input: payload.clone(),
            },
        );
        // Surface the card in the message list first: the question card
        // renders on the matching `ToolCall` item, so it must exist before
        // the authorization event parks the turn on the response.
        self.gate.emit(ThreadEvent::ToolCall {
            id: tool_call_id.to_string(),
            name: crate::tools::ASK_USER_QUESTION.to_string(),
            title: title.to_string(),
            status: ToolCallStatus::PendingApproval,
            input: Some(payload.clone()),
        });
        self.gate.emit(ThreadEvent::ToolCallAuthorization {
            id: tool_call_id.to_string(),
            tool_name: crate::tools::ASK_USER_QUESTION.to_string(),
            summary: title.to_string(),
            input: payload,
        });

        let response = tokio::select! {
            r = rx => r.unwrap_or(ToolAuthorizationResponse::Decision(PermissionDecision::Deny)),
            _ = signal.cancelled() => {
                self.gate.discard(tool_call_id);
                ToolAuthorizationResponse::Decision(PermissionDecision::Deny)
            }
        };
        // The pending responder is spent whether the UI answered or cancel
        // fired; remove it so a late respond cannot revive a settled call.
        self.gate.discard(tool_call_id);

        match response {
            ToolAuthorizationResponse::Decision(decision) => match decision {
                PermissionDecision::AllowOnce => UserVerdict::Allow,
                PermissionDecision::AlwaysAllow => UserVerdict::AlwaysAllow,
                PermissionDecision::Deny => UserVerdict::Deny,
            },
            // A selected option label is the user's explicit verdict and wins
            // over any supplemental text; a reply with no selection is not an
            // approval. Matched against the labels captured at card
            // construction above. Unrecognized answers fall through to Deny
            // by design: a conservative default, not a missed match arm.
            ToolAuthorizationResponse::AskUserQuestion { answers, .. } => {
                if answers
                    .iter()
                    .any(|(_, a)| a.as_str() == always_allow_label.as_str())
                {
                    UserVerdict::AlwaysAllow
                } else if answers
                    .iter()
                    .any(|(_, a)| a.as_str() == allow_once_label.as_str())
                {
                    UserVerdict::Allow
                } else {
                    UserVerdict::Deny
                }
            }
        }
    }
}

enum ReviewDisposition {
    Allowed,
    Escalate(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserVerdict {
    Allow,
    AlwaysAllow,
    Deny,
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

// ── AskUserQuestion (interactive, never approval-gated) ─────────────────────

/// The pi harness `AskUserQuestion` tool. Schema and semantics ported from
/// the retired manox tool: the run IS the round trip — the question card
/// renders from the `ToolCallAuthorization` event and the user's answers
/// come back through `respond_tool_authorization`, short-circuited into a
/// `ToolResult` without any execution. Read-only by contract: approval
/// modes, the reviewer, and the always-allow cache never touch it.
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

// ── Reviewer adapter: pi StreamFn ───────────────────────────────────────────

/// Adapt the session model's `StreamFn` (through the runtime resolver) to
/// the prompt-in/text-out reviewer seam. The reviewer call carries no tools
/// and no history — just the system prompt and the per-call payload.
pub fn pi_reviewer(runtime: ModelRuntime, model: PiModel) -> ReviewerFn {
    Arc::new(move |req: ReviewerRequest| {
        let runtime = runtime.clone();
        let model = model.clone();
        Box::pin(async move {
            let stream_fn = (runtime.resolver())(&model)?;
            let context = AgentContext {
                system_prompt: req.system,
                messages: vec![AgentMessage::User {
                    content: vec![ContentBlock::Text {
                        text: req.user,
                        signature: None,
                    }],
                    timestamp: chrono::Utc::now(),
                }],
                tools: Arc::new([]),
                model: model.clone(),
                thinking_level: None,
                cache_retention: CacheRetention::default(),
                session_id: None,
                stream_options: StreamOptions {
                    temperature: Some(0.0),
                    ..Default::default()
                },
                metadata: HashMap::new(),
            };
            // Drain the lifecycle events while the stream runs; the final
            // message (below) is authoritative, the drain just keeps the
            // channel from wedging the provider.
            let (tx, mut rx) = mpsc::channel(32);
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            let message = stream_fn
                .stream(&context, CancellationToken::new(), tx)
                .await?;
            let _ = drain.await;
            let AgentMessage::Assistant {
                content,
                model: response_model,
                usage,
                ..
            } = message
            else {
                anyhow::bail!("reviewer returned a non-assistant message");
            };
            let text: String = content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            Ok(ReviewerOutput {
                text,
                usage: Some(to_token_usage(&usage)),
                model_name: response_model,
            })
        })
    })
}

fn to_token_usage(u: &PiUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
    }
}

fn truncate_str(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let head_end = s.floor_char_boundary(max_bytes);
    format!("{}…", &s[..head_end])
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
        approval: bool,
        read_only: bool,
        ran: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PiAgentTool for MockTool {
        fn name(&self) -> &str {
            "Mock"
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

    fn gated(
        approval: bool,
        read_only: bool,
        gate: Arc<ApprovalGate>,
    ) -> (ApprovalGatedTool, Arc<AtomicUsize>) {
        let ran = Arc::new(AtomicUsize::new(0));
        let tool = ApprovalGatedTool::new(
            Arc::new(MockTool {
                approval,
                read_only,
                ran: Arc::clone(&ran),
            }),
            gate,
        );
        (tool, ran)
    }

    async fn drain_until_authorization(
        rx: &mut mpsc::UnboundedReceiver<BackendNotice>,
    ) -> (String, serde_json::Value) {
        while let Some(notice) = rx.recv().await {
            if let BackendNotice::Event(event) = notice
                && let ThreadEvent::ToolCallAuthorization { id, input, .. } = *event
            {
                return (id, input);
            }
        }
        panic!("no ToolCallAuthorization event arrived");
    }

    #[test]
    fn truncate_str_caps_at_char_boundary() {
        let s = "文".repeat(300); // 900 bytes
        let out = truncate_str(&s, 500);
        assert!(out.len() <= 501 + "…".len());
        assert!(out.ends_with('…'));
        assert!(out.is_char_boundary(out.len() - "…".len()));
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
    fn gate_register_respond_round_trip() {
        let gate = gate();
        let rx = gate.register(
            "call-1",
            PendingAuthMeta {
                tool_name: "Bash".into(),
                summary: "echo".into(),
                input: serde_json::json!({}),
            },
        );
        assert_eq!(gate.pending_entries().len(), 1);
        gate.respond(
            "call-1",
            ToolAuthorizationResponse::Decision(PermissionDecision::AlwaysAllow),
        );
        assert!(gate.pending_entries().is_empty());
        let mut rx = rx;
        let response = rx.try_recv().unwrap();
        assert!(matches!(
            response,
            ToolAuthorizationResponse::Decision(PermissionDecision::AlwaysAllow)
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
        assert_eq!(gate.mode(), ApprovalMode::AutoPilot);
        gate.set_mode(ApprovalMode::Danger);
        assert_eq!(gate.mode(), ApprovalMode::Danger);
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
        let (gate, mut rx) = {
            let (g, rx) = gate_with_events();
            (g, rx)
        };
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
    async fn danger_mode_runs_approval_tools_without_prompting() {
        let (gate, mut rx) = gate_with_events();
        gate.set_mode(ApprovalMode::Danger);
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
    async fn always_allow_cache_bypasses_the_gate() {
        let (gate, mut rx) = gate_with_events();
        gate.cache().set_always_allowed("Mock");
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
    async fn deny_verdict_returns_denied_result_without_running() {
        let (gate, mut rx) = gate_with_events();
        let (tool, ran) = gated(true, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let run = tokio::spawn(async move {
            tool.execute("c1", serde_json::json!({}), CancellationToken::new(), &ctx)
                .await
        });
        let (id, _input) = drain_until_authorization(&mut rx).await;
        assert_eq!(id, "c1");
        gate.respond(
            &id,
            ToolAuthorizationResponse::Decision(PermissionDecision::Deny),
        );
        let result = run.await.unwrap().unwrap();
        assert!(
            result.is_error,
            "denied call must surface as an error result"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 0, "denied call must not run");
    }

    #[tokio::test]
    async fn always_allow_grant_caches_for_the_rest_of_the_session() {
        let (gate, rx) = gate_with_events();
        let (tool, ran) = gated(true, false, Arc::clone(&gate));
        let ctx = tool_ctx();

        // First call prompts; a responder task plays the user and grants
        // AlwaysAllow while the gated execute parks on the oneshot.
        let responder = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                let mut rx = rx;
                let (id, _input) = drain_until_authorization(&mut rx).await;
                gate.respond(
                    &id,
                    ToolAuthorizationResponse::Decision(PermissionDecision::AlwaysAllow),
                );
                rx
            })
        };
        let result = tool
            .execute("c1", serde_json::json!({}), CancellationToken::new(), &ctx)
            .await
            .unwrap();
        let mut rx = responder.await.unwrap();
        assert!(!result.is_error);
        assert!(gate.cache().is_always_allowed("Mock"));

        // Second call rides the cache: the tool runs and no further
        // authorization prompt is emitted (queued card-restore events from
        // the first call are fine).
        let result = tool
            .execute("c2", serde_json::json!({}), CancellationToken::new(), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 2);
        while let Ok(notice) = rx.try_recv() {
            assert!(
                !matches!(&notice, BackendNotice::Event(event)
                    if matches!(**event, ThreadEvent::ToolCallAuthorization { .. })),
                "cached call must not prompt again"
            );
        }
    }

    #[tokio::test]
    async fn cancel_while_parked_denies_and_cleans_up() {
        let (gate, mut rx) = gate_with_events();
        let (tool, ran) = gated(true, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let signal = CancellationToken::new();
        let signal_for_call = signal.clone();
        let run = tokio::spawn(async move {
            tool.execute("c1", serde_json::json!({}), signal_for_call, &ctx)
                .await
        });
        let (id, _input) = drain_until_authorization(&mut rx).await;
        signal.cancel();
        let result = run.await.unwrap().unwrap();
        assert!(result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(
            gate.pending_entries().is_empty(),
            "cancelled call must drop its pending authorization"
        );
        // A late verdict for the cancelled call is ignored, not resurrected.
        gate.respond(
            &id,
            ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce),
        );
    }

    fn gated_monitor(gate: Arc<ApprovalGate>) -> ApprovalGatedTool {
        let manager = Arc::new(pi_extensions::monitor::MonitorManager::new(Arc::new(
            pi_extensions::BackgroundRegistry::new(),
        )));
        ApprovalGatedTool::new(
            Arc::new(pi_extensions::monitor::MonitorTool::new(manager)),
            gate,
        )
    }

    /// Gate-level proof that the `Monitor` command half rides the same gate
    /// as Bash: the gated execute parks on a user authorization before the
    /// tool body can spawn anything, and a denial prevents the spawn.
    #[tokio::test]
    async fn monitor_command_half_parks_on_gate() {
        let (gate, mut rx) = gate_with_events();
        let monitor = gated_monitor(Arc::clone(&gate));
        let ctx = tool_ctx();
        let run = tokio::spawn(async move {
            monitor
                .execute(
                    "c1",
                    serde_json::json!({"description": "d", "command": "sleep 5"}),
                    CancellationToken::new(),
                    &ctx,
                )
                .await
        });
        let (id, _input) = drain_until_authorization(&mut rx).await;
        assert_eq!(id, "c1");
        gate.respond(
            &id,
            ToolAuthorizationResponse::Decision(PermissionDecision::Deny),
        );
        let result = run.await.unwrap().unwrap();
        assert!(result.is_error, "a denied command monitor must not start");
    }

    /// Gate-level proof that the `Monitor` ws half is exempt: the gated
    /// execute delegates straight through with no authorization notice. The
    /// URL uses an unsupported scheme so the tool body fails fast on its own
    /// validation — the assertion is that the gate never parked the call.
    #[tokio::test]
    async fn monitor_ws_half_delegates_without_gate() {
        let (gate, mut rx) = gate_with_events();
        let monitor = gated_monitor(Arc::clone(&gate));
        let ctx = tool_ctx();
        let err = monitor
            .execute(
                "c1",
                serde_json::json!({"description": "d", "ws": {"url": "http://example.com"}}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("unsupported scheme"),
            "the tool body itself ran and rejected the scheme: {err}"
        );
        assert!(
            rx.try_recv().is_err(),
            "no authorization notice for the ws half"
        );
        assert!(gate.pending_entries().is_empty());
    }
}
