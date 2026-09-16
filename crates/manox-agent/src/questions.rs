//! The `AskUserQuestion` interactive round trip (host-layer tool).
//!
//! The pi kernel exposes the `requires_approval` seam on `AgentTool` but
//! ships no interactive ask surface — that is a host concern. This module is
//! the host's ask tool: it is interactive, never permission-gated. It parks
//! a question card on the user through the [`ApprovalGate`]'s pending
//! registry and folds the id-routed answers into a canonical JSON tool
//! result. The gate itself (mode-based allow/deny policy) lives in
//! `approval`; this tool only rides its register/emit/respond/discard
//! plumbing.

use std::sync::Arc;

use manox_harness::tool::{AgentTool as PiAgentTool, AgentToolResult, ToolContext, ToolError};
use tokio_util::sync::CancellationToken;

use crate::approval::ApprovalGate;
use crate::permission::{
    AskAnswer, PendingAuthMeta, PermissionDecision, ToolAuthorizationResponse,
};
use crate::thread::{ThreadEvent, ToolCallStatus};

/// The pi harness `AskUserQuestion` tool. Schema and semantics ported from
/// the retired manox tool: the run IS the round trip — the question card
/// renders from the `ToolCallAuthorization` event and the user's answers
/// come back through `respond_tool_authorization`, short-circuited into a
/// `ToolResult` without any execution. Read-only by contract: permission
/// modes never touch it.
pub struct PiAskUserQuestionTool {
    gate: Arc<ApprovalGate>,
    /// Live plan-mode flag, so a dismissed question card can tell the model
    /// to *stay in plan mode* when the user closes it mid-planning (dsh
    /// `ASK_CANCELLED` under `intent:plan-review`) versus the plain stop-and-
    /// wait line outside plan mode. `None` in bare test constructions → the
    /// general wording.
    plan: Option<Arc<crate::plan_mode::PlanSessionState>>,
}

impl PiAskUserQuestionTool {
    pub fn new(gate: Arc<ApprovalGate>) -> Self {
        Self { gate, plan: None }
    }

    /// Attach the session's live plan-mode state (host assembly).
    pub fn with_plan_state(mut self, plan: Arc<crate::plan_mode::PlanSessionState>) -> Self {
        self.plan = Some(plan);
        self
    }

    fn plan_mode_active(&self) -> bool {
        self.plan.as_ref().is_some_and(|p| p.enabled())
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
         Each question carries its options (any number), and may carry a stable \
         `id` (one is minted for you when omitted), a `detail` markdown support \
         text, and an `intent` ({kind, approve} — name the approving option \
         label in `approve`, and only when a `detail` accompanies the question). \
         Mark the recommended default with recommended=true when one exists. The \
         user may also answer a question with free text or explicitly skip it. \
         Do not use this tool to ask for plan approval or to confirm obvious \
         defaults."
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
        mut params: serde_json::Value,
        signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        // D5 `DELEGATED_CALLER` (dsh L4: only the runtime root may ask a human).
        // The approval gate is the host's human-facing service; a subagent runs
        // with a synthetic fail-closed gate (see `engine`'s subagent build), so
        // a bare `ApprovalGate` on the *main* line is the marker of the runtime
        // root. Any caller that is not the root is rejected before its question
        // can park on the human. This is defense-in-depth behind the
        // `select_tools` assembly strip (D5): today the child snapshot already
        // lacks this tool, so the guard closes the hole a future mis-assembly
        // or a name-padded custom definition would open.
        if self.gate.is_delegated() {
            return Ok(AgentToolResult::error(
                "[DELEGATED_CALLER] A delegated subagent cannot ask the user \
                 questions. Include the open question in your final summary so \
                 the parent agent — which can ask — resolves it.",
            ));
        }
        if let Err(err) = validate_ask_input(&params) {
            return Err(ToolError::InvalidArguments(err));
        }
        // B2-PR-1 (L1): every question carries a stable `id` the client's
        // answer routes by. A model that omits one gets it minted here — the
        // single point that runs before the request crosses the wire — so the
        // parked input, the delivered card, and the settled answer all speak
        // the same id vocabulary.
        ensure_ask_ids(&mut params);
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
            input: params.clone(),
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
            ToolAuthorizationResponse::AskUserQuestion { answers } => {
                // PR-2 (C3): the model-facing encoding is the CANONICAL JSON
                // line — the same vocabulary the client answered with:
                // `{"answers":[{"id":…,"selected":[…],"custom":…?},…]}`.
                // Skip is exactly `selected: []` with no `custom` (the
                // prose skip line and the localized Question:/Answer:
                // wrapper are gone: free text can only ever attach to its
                // own question, single-select custom replaces the
                // selection, multi-select custom supplements it — folded
                // by `fold_ask_answers`).
                let rows = fold_ask_answers(&params, answers);
                let text = serde_json::json!({ "answers": rows }).to_string();
                Ok(AgentToolResult::text(text))
            }
            // A lapsed delivery (no client able to answer, or a withdrawn
            // delivery) with NO human action. This is NOT an empty answer:
            // the text names that explicitly so the model re-asks or proceeds
            // under stated assumptions instead of reading silence as consent.
            ToolAuthorizationResponse::AskUserQuestionExpired => Ok(AgentToolResult::error(
                "[no-answer] The user did not answer this question — no client was \
                 available to answer it, or the pending question was withdrawn. Do not \
                 treat this as input or consent. Re-ask with fewer, simpler questions \
                 or continue under explicitly stated assumptions.",
            )),
            // The user closed the card to speak instead (dsh `ASK_CANCELLED`):
            // NOT an answer, NOT a rejection, NOT a turn interrupt. The model
            // stops and waits for the forthcoming message. In plan mode the
            // dsh line keeps the "stay in plan mode" clause; elsewhere the
            // same guidance drops it. Never re-asked as a denial.
            ToolAuthorizationResponse::AskUserQuestionDismissed => {
                let text = if self.plan_mode_active() {
                    "The user dismissed the review to speak instead; stay in plan mode, \
                     stop here, and wait for their message."
                } else {
                    "The user dismissed your questions to speak instead; stop here and \
                     wait for their message."
                };
                Ok(AgentToolResult::text(text))
            }
            // Any bare decision is a real rejection of the prompt — surface the
            // tool-denied render. A card *close* is no longer routed here (it
            // arrives as AskUserQuestionDismissed), so this arm is reject-only.
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

/// Mint a stable `id` onto every question that lacks one (B2-PR-1). Runs in
/// `execute` after validation and before the request is parked/emitted, so
/// the card the client renders and the answers routed back share one id
/// vocabulary. A model-supplied id (non-empty string) is preserved.
fn ensure_ask_ids(input: &mut serde_json::Value) {
    let Some(questions) = input.get_mut("questions").and_then(|v| v.as_array_mut()) else {
        return; // validation already rejected this shape
    };
    for question in questions.iter_mut() {
        let has_id = question
            .get("id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty());
        if !has_id {
            question["id"] = serde_json::Value::String(uuid::Uuid::new_v4().to_string());
        }
    }
}

/// Validate one question's optional L1 vocabulary (`id` / `detail` /
/// `intent`) plus its options. `BAD_INTENT` is the dsh report's code for an
/// intent that cannot mean what it says: an `approve` label that is not one
/// of this question's own options, or a declared `kind` with no `detail`
/// support text for the specialised surface to render.
fn validate_question_l1(idx: usize, question: &serde_json::Value) -> Result<(), String> {
    let at = |msg: String| format!("AskUserQuestion question {} {msg}", idx + 1);
    if let Some(id) = question.get("id")
        && !matches!(id, serde_json::Value::String(s) if !s.trim().is_empty())
    {
        return Err(at("carries a non-string or blank `id`".into()));
    }
    if question.get("detail").is_some()
        && !matches!(question["detail"], serde_json::Value::String(_))
    {
        return Err(at("`detail` must be a markdown string".into()));
    }
    // Options: any number (the old 2..=3 cap is gone with B2-PR-1), and the
    // list itself is optional — a detail/intent-only question is legal in the
    // canonical vocabulary.
    let mut labels: Vec<&str> = Vec::new();
    if let Some(options) = question.get("options") {
        let options = options
            .as_array()
            .ok_or_else(|| at("`options` must be an array".to_string()))?;
        for option in options {
            if let Some(label) = option.get("label").and_then(|v| v.as_str()) {
                labels.push(label);
            } else {
                return Err(at("every option needs a string `label`".into()));
            }
        }
    }
    if let Some(intent) = question.get("intent") {
        if !intent.is_object() {
            return Err(at("`intent` must be an object".into()));
        }
        let kind = intent.get("kind");
        if kind.is_some() && !matches!(kind, Some(serde_json::Value::String(_))) {
            return Err(at("BAD_INTENT: `intent.kind` must be a string".into()));
        }
        let approve = intent.get("approve");
        if approve.is_some()
            && !matches!(approve, Some(serde_json::Value::String(s)) if labels.contains(&s.as_str()))
        {
            return Err(at(
                "BAD_INTENT: `intent.approve` must name one of this question's option labels"
                    .into(),
            ));
        }
        let has_detail = matches!(question.get("detail"), Some(serde_json::Value::String(s)) if !s.trim().is_empty());
        if kind.is_some() && !has_detail {
            return Err(at(
                "BAD_INTENT: `intent.kind` requires a non-empty `detail`".into(),
            ));
        }
    }
    Ok(())
}

fn validate_ask_input(input: &serde_json::Value) -> Result<(), String> {
    let questions = input
        .get("questions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "AskUserQuestion requires a `questions` array".to_string())?;
    // B2-PR-1 (report §L1): the 1..=3 question-count cap is gone — any
    // non-empty list is valid.
    if questions.is_empty() {
        return Err("AskUserQuestion requires at least one question".to_string());
    }
    for (idx, question) in questions.iter().enumerate() {
        if !matches!(question.get("question"), Some(serde_json::Value::String(_))) {
            return Err(format!(
                "AskUserQuestion question {} needs `question`",
                idx + 1
            ));
        }
        validate_question_l1(idx, question)?;
    }
    Ok(())
}

fn ask_user_question_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "description": "The questions to ask the user (any number). Each becomes one step in the question drawer.",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Stable identifier the user's answer routes by. Optional: one is minted when omitted."
                        },
                        "question": {
                            "type": "string",
                            "description": "The full question text to display."
                        },
                        "detail": {
                            "type": "string",
                            "description": "Optional markdown support text rendered beneath the question (required when `intent.kind` is set)."
                        },
                        "intent": {
                            "type": "object",
                            "description": "Optional interaction intent. `kind` names the specialised surface (e.g. \"plan-review\"); `approve` must be one of this question's option labels — the option that means \"yes\" on that surface.",
                            "properties": {
                                "kind": {
                                    "type": "string",
                                    "description": "The intent kind, e.g. \"plan-review\"."
                                },
                                "approve": {
                                    "type": "string",
                                    "description": "The option label that carries the approval verdict."
                                }
                            }
                        },
                        "header": {
                            "type": "string",
                            "description": "Short label for the question (max 12 characters)."
                        },
                        "options": {
                            "type": "array",
                            "description": "Choices for the user to select from (any number; omit for a free-text-only question).",
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
                    "required": ["question", "header", "multiSelect"]
                }
            }
        },
        "required": ["questions"]
    })
}

/// B2-PR-1 / PR-2: fold the canonical id-routed tri-state answers into
/// the canonical model-facing JSON rows — one object per answered
/// question, `{"id", "selected": [label…], "custom"?: string}`, serialized
/// under the top-level `{"answers": [...]}` by the caller. Routing is by
/// `id` against the parked request input: an answer whose id matches no
/// question is dropped (the client answered a card that is not this
/// call's). Faithfulness rules (unchanged from the transitional renderer,
/// now expressed in the vocabulary itself):
/// - `selected` non-empty + no custom → the labels.
/// - custom on a SINGLE-select question → REPLACES the selection
///   (`selected: []` + the text).
/// - custom on a MULTI-select question alongside selections →
///   SUPPLEMENTS: labels plus `custom`.
/// - multi-select, nothing picked, + custom → the text is the whole
///   answer (`selected: []` + `custom`).
/// - empty selection + no/blank custom → an explicit skip:
///   `selected: []` with NO `custom`.
fn fold_ask_answers(
    parked_input: &serde_json::Value,
    answers: Vec<AskAnswer>,
) -> Vec<serde_json::Value> {
    let questions = parked_input
        .get("questions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    for answer in answers {
        let Some(question) = questions
            .iter()
            .find(|q| q.get("id").and_then(|v| v.as_str()).unwrap_or("") == answer.id)
        else {
            continue; // id-routed: an answer for another card settles nothing here
        };
        let multi = question
            .get("multiSelect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut row = serde_json::json!({
            "id": answer.id,
            "selected": answer.selected,
        });
        if let Some(custom) = answer.custom {
            // Single-select custom replaces the selection outright
            // (dsh L6.3); multi-select custom supplements it.
            if !multi {
                row["selected"] = serde_json::Value::Array(Vec::new());
            }
            row["custom"] = serde_json::Value::String(custom);
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_engine::BackendNotice;
    use manox_harness::env::TokioExecutionEnv;
    use manox_harness::tool::{LocalToolContext, ToolState};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

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

    #[test]
    fn validate_ask_input_drops_the_batch1_caps() {
        // One question, two options — the historical minimum still passes.
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

        // B2-PR-1: four options (was rejected by the 2..=3 cap).
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
        assert!(
            validate_ask_input(&four_options).is_ok(),
            "option-count cap removed: {:?}",
            validate_ask_input(&four_options)
        );

        // B2-PR-1: five questions (was rejected by the 1..=3 cap).
        let many_questions: Vec<serde_json::Value> = (0..5)
            .map(|i| {
                serde_json::json!({
                    "question": format!("q{i}"), "header": "h", "multiSelect": false,
                    "options": [
                        {"label": "a", "description": ""},
                        {"label": "b", "description": ""}
                    ]
                })
            })
            .collect();
        assert!(validate_ask_input(&serde_json::json!({ "questions": many_questions })).is_ok());

        // Empty list and missing array still fail.
        assert!(validate_ask_input(&serde_json::json!({ "questions": [] })).is_err());
        let no_questions = serde_json::json!({});
        assert!(validate_ask_input(&no_questions).is_err());
    }

    #[test]
    fn ask_schema_has_no_counts_and_declares_the_l1_vocabulary() {
        let schema = ask_user_question_schema();
        let questions = &schema["properties"]["questions"];
        assert_eq!(questions["minItems"], 1);
        assert!(questions.get("maxItems").is_none(), "question cap removed");
        let options = &questions["items"]["properties"]["options"];
        assert!(options.get("minItems").is_none(), "option floor removed");
        assert!(options.get("maxItems").is_none(), "option cap removed");
        // options is OPTIONAL in the canonical vocabulary.
        let required = questions["items"]["required"]
            .as_array()
            .expect("items have required");
        assert!(
            !required.iter().any(|r| r == "options"),
            "options no longer mandatory: {required:?}"
        );
        // id / detail / intent are advertised to the model.
        for field in ["id", "detail", "intent"] {
            assert!(
                questions["items"]["properties"].get(field).is_some(),
                "schema carries {field}"
            );
        }
        assert_eq!(
            questions["items"]["properties"]["intent"]["properties"]["kind"]["type"],
            "string"
        );
    }

    #[test]
    fn intent_validation_bad_intent_cases() {
        // `approve` must name one of THIS question's option labels.
        let stray_approve = serde_json::json!({
            "questions": [{
                "question": "q", "header": "h", "multiSelect": false,
                "options": [
                    {"label": "a", "description": ""},
                    {"label": "b", "description": ""}
                ],
                "intent": {"kind": "plan-review", "approve": "Approve"},
                "detail": "# the plan"
            }]
        });
        let err = validate_ask_input(&stray_approve).unwrap_err();
        assert!(err.contains("BAD_INTENT"), "approve not an option: {err}");

        // `intent.kind` set without `detail`.
        let kind_without_detail = serde_json::json!({
            "questions": [{
                "question": "q", "header": "h", "multiSelect": false,
                "options": [
                    {"label": "a", "description": ""},
                    {"label": "b", "description": ""}
                ],
                "intent": {"kind": "plan-review", "approve": "a"}
            }]
        });
        let err = validate_ask_input(&kind_without_detail).unwrap_err();
        assert!(err.contains("BAD_INTENT"), "kind needs detail: {err}");

        // Well-formed intent passes.
        let good = serde_json::json!({
            "questions": [{
                "question": "q", "header": "h", "multiSelect": false,
                "options": [
                    {"label": "a", "description": ""},
                    {"label": "b", "description": ""}
                ],
                "intent": {"kind": "plan-review", "approve": "a"},
                "detail": "# the plan body"
            }]
        });
        assert!(validate_ask_input(&good).is_ok());
    }

    #[test]
    fn ensure_ask_ids_mints_missing_and_keeps_provided() {
        let mut input = serde_json::json!({
            "questions": [
                {"question": "q1"},
                {"question": "q2", "id": "stable-42"}
            ]
        });
        ensure_ask_ids(&mut input);
        let questions = input["questions"].as_array().unwrap();
        let id0 = questions[0]["id"].as_str().expect("id minted");
        assert!(!id0.trim().is_empty(), "minted id is non-empty");
        assert_ne!(id0, "q1", "the minted id is a uuid, not the question text");
        // Parses as a v4 uuid.
        assert_eq!(
            uuid::Uuid::parse_str(id0).ok().map(|u| u.get_version_num()),
            Some(4),
            "minted ids are uuidv4: {id0}"
        );
        assert_eq!(
            questions[1]["id"], "stable-42",
            "a model-supplied id is kept"
        );

        // Idempotent: a second pass changes nothing.
        let before = input.clone();
        ensure_ask_ids(&mut input);
        assert_eq!(before, input);
    }

    /// PR-2 (C3): the fold produces the canonical JSON rows — skip is
    /// `selected: []` without a `custom`, single-select custom replaces the
    /// selection, multi-select custom supplements it, unknown ids drop.
    #[test]
    fn fold_ask_answers_produces_canonical_json_rows() {
        let parked = serde_json::json!({
            "questions": [
                {"id": "s1", "question": "single?", "header": "h", "multiSelect": false,
                 "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}]},
                {"id": "m1", "question": "multi?", "header": "h", "multiSelect": true,
                 "options": [{"label": "x", "description": ""}, {"label": "y", "description": ""}]},
                {"id": "k1", "question": "skip?", "header": "h", "multiSelect": false,
                 "options": [{"label": "p", "description": ""}, {"label": "q", "description": ""}]}
            ]
        });
        let rows = fold_ask_answers(
            &parked,
            vec![
                // Multi-select: selections + custom → labels answer, custom supplements.
                AskAnswer::new(
                    "m1".into(),
                    vec!["x".into(), "y".into()],
                    Some("and also z".into()),
                ),
                // Single-select with a selection: labels only.
                AskAnswer::new("s1".into(), vec!["a".into()], None),
                // Explicit skip: empty selection, no custom.
                AskAnswer::new("k1".into(), vec![], None),
                // Unknown id: dropped.
                AskAnswer::new("nope".into(), vec!["a".into()], None),
            ],
        );
        assert_eq!(
            rows,
            vec![
                serde_json::json!({"id": "m1", "selected": ["x", "y"], "custom": "and also z"}),
                serde_json::json!({"id": "s1", "selected": ["a"]}),
                // Skip: empty `selected`, NO `custom` key — no prose line.
                serde_json::json!({"id": "k1", "selected": []}),
            ],
            "the unknown-id answer is dropped and the tri-state speaks the canonical shape"
        );

        // Single-select custom REPLACES the selection (dsh L6.3).
        let rows = fold_ask_answers(
            &parked,
            vec![AskAnswer::new(
                "s1".into(),
                vec!["a".into()],
                Some("typed".into()),
            )],
        );
        assert_eq!(
            rows[0],
            serde_json::json!({"id": "s1", "selected": [], "custom": "typed"}),
            "custom overrides the selection, leaving `selected` empty"
        );

        // Multi-select, nothing picked + custom → the text is the whole answer.
        let rows = fold_ask_answers(
            &parked,
            vec![AskAnswer::new("m1".into(), vec![], Some("free".into()))],
        );
        assert_eq!(
            rows[0],
            serde_json::json!({"id": "m1", "selected": [], "custom": "free"})
        );

        // Blank custom normalises (via AskAnswer::new) to an explicit skip.
        let rows = fold_ask_answers(
            &parked,
            vec![AskAnswer::new("k1".into(), vec![], Some("   ".into()))],
        );
        assert_eq!(
            rows[0],
            serde_json::json!({"id": "k1", "selected": []}),
            "blank custom collapses to skip"
        );
    }

    /// PR-2 (C3): the tool's success result is ONE canonical JSON line —
    /// `{"answers": [...]}` — with no wrapper prose; the model sees exactly
    /// the vocabulary the client answered with.
    #[tokio::test]
    async fn ask_success_result_is_the_canonical_json_line() {
        let (gate, _rx) = gate_with_events();
        let tool = PiAskUserQuestionTool::new(Arc::clone(&gate));
        let ctx = tool_ctx();
        let params = serde_json::json!({
            "questions": [{
                "id": "shape-q", "question": "shape?", "header": "h", "multiSelect": false,
                "options": [
                    {"label": "round", "description": ""},
                    {"label": "square", "description": ""},
                ],
            }],
        });
        let settle = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                while !gate.pending_entries().iter().any(|(id, _)| id == "ask-1") {
                    tokio::task::yield_now().await;
                }
                gate.respond(
                    "ask-1",
                    ToolAuthorizationResponse::AskUserQuestion {
                        answers: vec![AskAnswer::new("shape-q".into(), vec!["round".into()], None)],
                    },
                );
            })
        };
        let result = tool
            .execute("ask-1", params, CancellationToken::new(), &ctx)
            .await
            .expect("the ask tool returns a tool result, not a hard error");
        settle.await.unwrap();
        assert!(!result.is_error);
        let text = result_text(&result);
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("the success result is one JSON line: {text}");
        assert_eq!(
            parsed,
            serde_json::json!({"answers": [{"id": "shape-q", "selected": ["round"]}]})
        );
    }

    /// R1: an adjudication settled without user input (`Expired`) reaches
    /// the model as an explicit `[no-answer]` error, not an empty answer
    /// text that reads as consent.
    #[tokio::test]
    async fn ask_expired_adjudication_answers_no_answer() {
        let (gate, _rx) = gate_with_events();
        let tool = PiAskUserQuestionTool::new(Arc::clone(&gate));
        let ctx = tool_ctx();
        let params = serde_json::json!({
            "questions": [{
                "question": "q", "header": "h", "multiSelect": false,
                "options": [
                    {"label": "a", "description": ""},
                    {"label": "b", "description": ""},
                ],
            }],
        });
        let settle = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                while !gate.pending_entries().iter().any(|(id, _)| id == "ask-1") {
                    tokio::task::yield_now().await;
                }
                gate.respond("ask-1", ToolAuthorizationResponse::AskUserQuestionExpired);
            })
        };
        let result = tool
            .execute("ask-1", params, CancellationToken::new(), &ctx)
            .await
            .unwrap();
        settle.await.unwrap();
        assert!(result.is_error, "expired must not read as a plain answer");
        let text = match &result.content[0] {
            manox_harness::types::ContentBlock::Text { text, .. } => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("[no-answer]"), "expired verdict text: {text}");
    }

    /// Extract the single text block of a tool result.
    fn result_text(result: &AgentToolResult) -> String {
        match &result.content[0] {
            manox_harness::types::ContentBlock::Text { text, .. } => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    fn ask_params() -> serde_json::Value {
        serde_json::json!({
            "questions": [{
                "question": "q", "header": "h", "multiSelect": false,
                "options": [
                    {"label": "a", "description": ""},
                    {"label": "b", "description": ""},
                ],
            }],
        })
    }

    /// Drive the ask tool and settle its parked card with `response`.
    async fn run_ask_settled(
        tool: &PiAskUserQuestionTool,
        gate: &Arc<ApprovalGate>,
        ctx: &LocalToolContext,
        response: ToolAuthorizationResponse,
    ) -> AgentToolResult {
        let settle = {
            let gate = Arc::clone(gate);
            tokio::spawn(async move {
                while !gate.pending_entries().iter().any(|(id, _)| id == "ask-1") {
                    tokio::task::yield_now().await;
                }
                gate.respond("ask-1", response);
            })
        };
        let result = tool
            .execute("ask-1", ask_params(), CancellationToken::new(), ctx)
            .await
            .expect("the ask tool returns a tool result, not a hard error");
        settle.await.unwrap();
        result
    }

    /// PR-0b: a card the user CLOSED to speak (`Dismissed`) is a distinct,
    /// non-rejection, non-timeout outcome. Outside plan mode it tells the
    /// model to stop and wait for the message — with no "plan mode" clause and
    /// NOT the `[no-answer]`/denial text the old conflation produced.
    #[tokio::test]
    async fn ask_dismissed_outside_plan_mode_waits_for_message() {
        let (gate, _rx) = gate_with_events();
        let tool = PiAskUserQuestionTool::new(Arc::clone(&gate));
        let ctx = tool_ctx();
        let result = run_ask_settled(
            &tool,
            &gate,
            &ctx,
            ToolAuthorizationResponse::AskUserQuestionDismissed,
        )
        .await;
        assert!(!result.is_error, "a dismissal is guidance, not an error");
        let text = result_text(&result);
        assert!(
            text.contains("dismissed") && text.contains("wait for their message"),
            "dismissal verdict text: {text}"
        );
        assert!(
            !text.contains("plan mode"),
            "general line has no plan clause: {text}"
        );
        assert!(
            !text.contains("[no-answer]") && !text.contains("denied"),
            "dismissal must not read as expiry or denial: {text}"
        );
    }

    /// PR-0b: inside plan mode the dsh "stay in plan mode" clause is present,
    /// so the model keeps drafting rather than exiting or re-asking.
    #[tokio::test]
    async fn ask_dismissed_in_plan_mode_keeps_plan_clause() {
        let (gate, _rx) = gate_with_events();
        let plan = crate::plan_mode::PlanSessionState::new();
        plan.set(true, None);
        let tool = PiAskUserQuestionTool::new(Arc::clone(&gate)).with_plan_state(plan);
        let ctx = tool_ctx();
        let result = run_ask_settled(
            &tool,
            &gate,
            &ctx,
            ToolAuthorizationResponse::AskUserQuestionDismissed,
        )
        .await;
        assert!(!result.is_error);
        let text = result_text(&result);
        assert!(
            text.contains("stay in plan mode") && text.contains("wait for their message"),
            "plan-mode dismissal keeps the stay clause: {text}"
        );
    }

    /// B2-PR-1 end-to-end at the tool seam: the minted `id` is visible on
    /// BOTH the delivered authorization input and the re-surfaced pending
    /// card, and an answer routed by that id settles as a rendered answer.
    #[tokio::test]
    async fn ask_mints_id_on_the_card_and_routes_the_answer_by_id() {
        let (gate, mut notices) = gate_with_events();
        let tool = PiAskUserQuestionTool::new(Arc::clone(&gate));
        let ctx = tool_ctx();
        let settle = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                while !gate.pending_entries().iter().any(|(id, _)| id == "ask-1") {
                    tokio::task::yield_now().await;
                }
                let input = gate
                    .pending_entries()
                    .into_iter()
                    .find(|(id, _)| id == "ask-1")
                    .expect("parked card")
                    .1
                    .input;
                let qid = input["questions"][0]["id"]
                    .as_str()
                    .expect("the parked card carries a minted id")
                    .to_string();
                assert!(
                    uuid::Uuid::parse_str(&qid).is_ok(),
                    "the minted id is a uuid: {qid}"
                );
                gate.respond(
                    "ask-1",
                    ToolAuthorizationResponse::AskUserQuestion {
                        answers: vec![AskAnswer::new(qid, vec!["a".into()], None)],
                    },
                );
            })
        };
        let result = tool
            .execute("ask-1", ask_params(), CancellationToken::new(), &ctx)
            .await
            .unwrap();
        settle.await.unwrap();
        assert!(!result.is_error, "an id-routed answer renders as an answer");
        // The authorization event delivered to the client carried the minted
        // id in its input (the same id the settle above read back).
        let mut card_ids = Vec::new();
        while let Ok(notice) = notices.try_recv() {
            if let BackendNotice::Event(event) = notice
                && let ThreadEvent::ToolCallAuthorization { input, .. } = *event
            {
                card_ids.extend(
                    input["questions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|q| q["id"].as_str().unwrap_or("").to_string()),
                );
            }
        }
        assert_eq!(card_ids.len(), 1, "one authorization event");
        assert!(uuid::Uuid::parse_str(&card_ids[0]).is_ok());
        // PR-2 (C3): the settle arrives as the canonical JSON line, routed
        // by the minted id — the selected label, language-independent, is
        // what the model reads.
        let parsed: serde_json::Value =
            serde_json::from_str(&result_text(&result)).expect("canonical JSON line");
        assert_eq!(
            parsed["answers"][0]["selected"][0], "a",
            "the id-routed answer is the settle payload"
        );
        assert_eq!(parsed["answers"][0]["id"], card_ids[0]);
    }

    /// D5 `DELEGATED_CALLER`: the ask tool refuses to park a human when it is
    /// handed a delegated (subagent) gate — it returns before registering any
    /// pending interaction or emitting an authorization event.
    #[tokio::test]
    async fn ask_from_delegated_gate_is_rejected_as_delegated_caller() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gate = Arc::new(ApprovalGate::new(tx, Arc::new(Mutex::new(None))).with_delegated(true));
        let tool = PiAskUserQuestionTool::new(Arc::clone(&gate));
        let ctx = tool_ctx();
        let result = tool
            .execute("ask-1", ask_params(), CancellationToken::new(), &ctx)
            .await
            .expect("returns a tool result, not a hard error");
        assert!(result.is_error, "a delegated call is an error to the model");
        let text = result_text(&result);
        assert!(
            text.contains("[DELEGATED_CALLER]"),
            "names the dsh taxonomy code: {text}"
        );
        assert!(
            gate.pending_entries().is_empty(),
            "no human card was parked by a delegated caller"
        );
        assert!(
            rx.try_recv().is_err(),
            "no authorization round trip was emitted"
        );
    }

    /// W1 byte-freeze: the model-visible surface of `AskUserQuestion`. The
    /// description and the parameter schema travel inside the cached prefix of
    /// every request, so a later work package may not move a single byte of
    /// them (nor of the answer row it produces) without a conscious update to
    /// the literals below.
    #[test]
    fn ask_surface_bytes_are_frozen() {
        let (gate, _rx) = gate_with_events();
        let tool = PiAskUserQuestionTool::new(gate);
        assert_eq!(
            tool.description(),
            "Ask the user clarifying questions when multiple valid approaches exist \
             and the answer changes what you do next. Use only for decisions that are \
             genuinely the user's to make — not for facts you can verify yourself. \
             Each question carries its options (any number), and may carry a stable \
             `id` (one is minted for you when omitted), a `detail` markdown support \
             text, and an `intent` ({kind, approve} — name the approving option \
             label in `approve`, and only when a `detail` accompanies the question). \
             Mark the recommended default with recommended=true when one exists. The \
             user may also answer a question with free text or explicitly skip it. \
             Do not use this tool to ask for plan approval or to confirm obvious \
             defaults."
        );
        assert_eq!(
            serde_json::to_string(&tool.parameters_schema()).unwrap(),
            r##"{"type":"object","properties":{"questions":{"type":"array","description":"The questions to ask the user (any number). Each becomes one step in the question drawer.","minItems":1,"items":{"type":"object","properties":{"id":{"type":"string","description":"Stable identifier the user's answer routes by. Optional: one is minted when omitted."},"question":{"type":"string","description":"The full question text to display."},"detail":{"type":"string","description":"Optional markdown support text rendered beneath the question (required when `intent.kind` is set)."},"intent":{"type":"object","description":"Optional interaction intent. `kind` names the specialised surface (e.g. \"plan-review\"); `approve` must be one of this question's option labels — the option that means \"yes\" on that surface.","properties":{"kind":{"type":"string","description":"The intent kind, e.g. \"plan-review\"."},"approve":{"type":"string","description":"The option label that carries the approval verdict."}}},"header":{"type":"string","description":"Short label for the question (max 12 characters)."},"options":{"type":"array","description":"Choices for the user to select from (any number; omit for a free-text-only question).","items":{"type":"object","properties":{"label":{"type":"string","description":"Concise label for the choice (1–5 words)."},"description":{"type":"string","description":"Explanation of what the choice means or implies."},"recommended":{"type":"boolean","description":"Whether this option is the recommended default."}},"required":["label","description"]}},"multiSelect":{"type":"boolean","description":"When true, the user may select multiple options; otherwise exactly one."}},"required":["question","header","multiSelect"]}}},"required":["questions"]}"##
        );
    }

    /// W1 byte-freeze: the result strings `AskUserQuestion` hands back to the
    /// model — the canonical answer row (key order and skip spelling included)
    /// and every non-answer verdict, byte for byte.
    #[tokio::test]
    async fn ask_result_bytes_are_frozen() {
        let (gate, _rx) = gate_with_events();
        let tool = PiAskUserQuestionTool::new(Arc::clone(&gate));
        let ctx = tool_ctx();
        let params = serde_json::json!({
            "questions": [{
                "id": "frozen-q", "question": "q?", "header": "h", "multiSelect": false,
                "options": [
                    {"label": "a", "description": ""},
                    {"label": "b", "description": ""},
                ],
            }],
        });
        let settle = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                while !gate.pending_entries().iter().any(|(id, _)| id == "ask-1") {
                    tokio::task::yield_now().await;
                }
                gate.respond(
                    "ask-1",
                    ToolAuthorizationResponse::AskUserQuestion {
                        answers: vec![
                            AskAnswer::new("frozen-q".into(), vec!["a".into()], None),
                            AskAnswer::new(
                                "frozen-q".into(),
                                vec!["b".into()],
                                Some("free".into()),
                            ),
                        ],
                    },
                );
            })
        };
        let answered = tool
            .execute("ask-1", params, CancellationToken::new(), &ctx)
            .await
            .expect("the ask tool returns a tool result, not a hard error");
        settle.await.unwrap();
        assert!(!answered.is_error);
        assert_eq!(
            result_text(&answered),
            r#"{"answers":[{"id":"frozen-q","selected":["a"]},{"id":"frozen-q","selected":[],"custom":"free"}]}"#,
            "one row per answer, `id` then `selected` then `custom`, single-select \
             custom replacing the selection"
        );

        let (gate, _rx) = gate_with_events();
        let tool = PiAskUserQuestionTool::new(Arc::clone(&gate));
        let expired = run_ask_settled(
            &tool,
            &gate,
            &ctx,
            ToolAuthorizationResponse::AskUserQuestionExpired,
        )
        .await;
        assert!(expired.is_error);
        assert_eq!(
            result_text(&expired),
            "[no-answer] The user did not answer this question — no client was \
             available to answer it, or the pending question was withdrawn. Do not \
             treat this as input or consent. Re-ask with fewer, simpler questions \
             or continue under explicitly stated assumptions."
        );

        let (gate, _rx) = gate_with_events();
        let tool = PiAskUserQuestionTool::new(Arc::clone(&gate));
        let dismissed = run_ask_settled(
            &tool,
            &gate,
            &ctx,
            ToolAuthorizationResponse::AskUserQuestionDismissed,
        )
        .await;
        assert!(!dismissed.is_error);
        assert_eq!(
            result_text(&dismissed),
            "The user dismissed your questions to speak instead; stop here and \
             wait for their message."
        );

        let (gate, _rx) = gate_with_events();
        let plan = crate::plan_mode::PlanSessionState::new();
        plan.set(true, None);
        let plan_tool = PiAskUserQuestionTool::new(Arc::clone(&gate)).with_plan_state(plan);
        let dismissed_in_plan = run_ask_settled(
            &plan_tool,
            &gate,
            &ctx,
            ToolAuthorizationResponse::AskUserQuestionDismissed,
        )
        .await;
        assert_eq!(
            result_text(&dismissed_in_plan),
            "The user dismissed the review to speak instead; stay in plan mode, \
             stop here, and wait for their message."
        );

        let (tx, _rx) = mpsc::unbounded_channel();
        let delegated_gate =
            Arc::new(ApprovalGate::new(tx, Arc::new(Mutex::new(None))).with_delegated(true));
        let delegated = PiAskUserQuestionTool::new(delegated_gate);
        let refused = delegated
            .execute("ask-1", ask_params(), CancellationToken::new(), &ctx)
            .await
            .expect("a delegated refusal is a tool result, not a hard error");
        assert!(refused.is_error);
        assert_eq!(
            result_text(&refused),
            "[DELEGATED_CALLER] A delegated subagent cannot ask the user questions. \
             Include the open question in your final summary so the parent agent — \
             which can ask — resolves it."
        );
    }
}
