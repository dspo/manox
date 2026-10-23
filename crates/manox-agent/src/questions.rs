//! The `AskUserQuestion` interactive round trip (host-layer tool).
//!
//! The user-questions seam: the host's `AskUserQuestion` card and the
//! pending registry the delivered card settles against.
//!
//! The kernel exposes the `requires_approval` seam on `AgentTool` but
//! ships no interactive ask surface — that is a host concern. This module owns
//! it end to end: the tool parses and validates the model's questions,
//! delivers one card per call on [`UserQuestionGate`], and ends the turn
//! (`terminate`) — the model never waits on the answer. The user's answer
//! arrives out-of-band as the seed of the NEXT turn, routed by card id
//! through the gateway. The gate is
//! this seam's settlement registry; the approval gate and its mode policy
//! live in `approval` and are not involved.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use manox_harness::tool::{AgentTool as HarnessAgentTool, AgentToolResult, ToolContext, ToolError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::permission::{AskAnswer, PendingAuthMeta};
use crate::thread::{ThreadEvent, ToolCallStatus};
use crate::thread_engine::{BackendNotice, send_notice};

/// How one delivered question settled. The question seam's own vocabulary.
#[derive(Debug)]
pub enum AskOutcome {
    /// The canonical id-routed tri-state answers, one per answered or
    /// explicitly skipped question.
    Answered(Vec<AskAnswer>),
    /// The user CLOSED the card to speak instead: an explicit "not now, let
    /// me talk" that is neither an answer nor a rejection. The card is
    /// withdrawn and the user's next message drives the next turn.
    Dismissed,
    /// The turn was cancelled with the card still open; the decision
    /// journals `cancelled` so no replay resurrects the card.
    Cancelled,
}

/// One delivered question: the card metadata the workspace re-surfaces
/// after a thread switch and the answer seed folds against.
struct PendingQuestion {
    meta: PendingAuthMeta,
}

/// The user-questions seam's delivery registry: the ask tool delivers here
/// and the gateway settles here through `Thread::respond_question`. One gate
/// per session; the delegated flag mirrors the approval gate's, so a
/// subagent caller is refused before any card is delivered.
pub struct UserQuestionGate {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    pending: Mutex<HashMap<String, PendingQuestion>>,
    delegated: bool,
}

impl UserQuestionGate {
    pub fn new(notice_tx: mpsc::UnboundedSender<BackendNotice>) -> Self {
        Self {
            notice_tx,
            pending: Mutex::new(HashMap::new()),
            delegated: false,
        }
    }

    /// Mark this gate as backing a delegated (subagent) caller rather than the
    /// runtime root.
    pub fn with_delegated(mut self, delegated: bool) -> Self {
        self.delegated = delegated;
        self
    }

    pub fn is_delegated(&self) -> bool {
        self.delegated
    }

    fn emit(&self, event: ThreadEvent) {
        send_notice(
            &self.notice_tx,
            BackendNotice::Event(Box::new(event)),
            "question-flow notice",
        );
    }

    /// Deliver one question: stores the card metadata under the call id.
    fn register(&self, id: &str, meta: PendingAuthMeta) {
        self.pending
            .lock()
            .unwrap()
            .insert(id.to_string(), PendingQuestion { meta });
    }

    /// Re-register a replayed card only when the id is not already open (the
    /// approval gate's same rule: a re-open racing a live card keeps the
    /// live one).
    pub(crate) fn register_if_absent(&self, id: &str, meta: PendingAuthMeta) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if pending.contains_key(id) {
            return false;
        }
        pending.insert(id.to_string(), PendingQuestion { meta });
        true
    }

    /// Withdraw one open question, returning its card metadata. `None` =
    /// unknown id (already settled or never delivered) — the first settle
    /// wins. The caller journals the decision durably: at the engine actor,
    /// where the idle append point is, in journal order ahead of any turn
    /// boundary the settle implies.
    pub(crate) fn withdraw(&self, id: &str) -> Option<PendingAuthMeta> {
        self.pending.lock().unwrap().remove(id).map(|p| p.meta)
    }

    /// Withdraw every open question (a dying turn's cleanup); the caller
    /// journals each as `cancelled`.
    pub(crate) fn withdraw_all(&self) -> Vec<(String, PendingAuthMeta)> {
        let mut pending = self.pending.lock().unwrap();
        pending.drain().map(|(id, p)| (id, p.meta)).collect()
    }

    /// Open questions with their card metadata, so the workspace can
    /// re-surface a card after switching back to a parked thread.
    pub fn pending_entries(&self) -> Vec<(String, PendingAuthMeta)> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .map(|(id, pending)| (id.clone(), pending.meta.clone()))
            .collect()
    }
}

/// The harness `AskUserQuestion` tool. The call DELIVERS the question card
/// and ends the turn (`terminate`): the card renders from the
/// `ToolCallAuthorization` event, the user's answer arrives out-of-band as
/// the next turn's seed, and the model never waits on it. Read-only by
/// contract: permission modes never touch it.
pub struct AskUserQuestionTool {
    gate: Arc<UserQuestionGate>,
}

impl AskUserQuestionTool {
    pub fn new(gate: Arc<UserQuestionGate>) -> Self {
        Self { gate }
    }
}

#[async_trait::async_trait]
impl HarnessAgentTool for AskUserQuestionTool {
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
         Calling this tool ENDS your turn: the card is the last thing you emit, \
         and the user's answers arrive as their next message. "
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
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        // D5 `DELEGATED_CALLER` (only the runtime root may ask a human).
        // The approval gate is the host's human-facing service; a subagent runs
        // with a synthetic fail-closed gate (see `engine`'s subagent build), so
        // a bare `ApprovalGate` on the *main* line is the marker of the runtime
        // root. Any caller that is not the root is rejected before its question
        // can reach the human. This is defense-in-depth behind the
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
        // English fallback title. Clients that render their own chrome may
        // substitute a display title; this value is what the model and any
        // non-localizing consumer see, and is never translated here.
        let title = CLARIFY_TITLE.to_string();

        self.gate.register(
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
        Ok(AgentToolResult {
            content: vec![manox_harness::types::ContentBlock::Text {
                text: ASK_DELIVERED_TEXT.to_string(),
                signature: None,
            }],
            details: Some(serde_json::json!({ "delivered": true })),
            is_error: false,
            usage: None,
            added_tool_names: None,
            // The turn ends at the delivery: the card stays the
            // conversation's last item and the answer seeds the NEXT turn —
            // the model never waits on a parked result.
            terminate: true,
        })
    }
}

/// The model-facing tool result of a delivered ask. English, never
/// localized.
const ASK_DELIVERED_TEXT: &str = "Question delivered to the user. This turn ends here — the \
     card is the conversation's last item. The user's answers arrive as their next message \
     (a closed card means they will speak instead); do not repeat or answer the questions \
     yourself.";

/// The model-facing seed of the turn that opens after an answered ask: one
/// prose line naming its origin, then the canonical answers JSON line — the
/// same vocabulary the tool result used to carry.
pub fn answer_seed_payload(input: &serde_json::Value, answers: Vec<AskAnswer>) -> String {
    let rows = fold_ask_answers(input, answers);
    format!(
        "The user answered your AskUserQuestion card:\n{}",
        serde_json::json!({ "answers": rows })
    )
}

/// The journal verdict word for a settled ask (`question` decision rows).
pub fn ask_verdict(outcome: &AskOutcome) -> &'static str {
    match outcome {
        AskOutcome::Answered(_) => "answered",
        AskOutcome::Dismissed => "dismissed",
        AskOutcome::Cancelled => "cancelled",
    }
}

/// Fallback display title for an `AskUserQuestion` tool call whose own
/// `header`/summary is empty. English, never localized: it travels to clients
/// as model-facing tool-call metadata.
const CLARIFY_TITLE: &str = "Clarifying question";

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
/// `intent`) plus its options. `BAD_INTENT` is the code for an
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
            // Single-select custom replaces the selection outright;
            // multi-select custom supplements it.
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
    use tokio::sync::mpsc;

    fn gate_with_events() -> (
        Arc<UserQuestionGate>,
        mpsc::UnboundedReceiver<BackendNotice>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(UserQuestionGate::new(tx)), rx)
    }

    fn tool_ctx() -> LocalToolContext {
        LocalToolContext::new(
            Arc::new(TokioExecutionEnv::new(std::env::temp_dir())),
            std::env::temp_dir(),
            Arc::new(ToolState::new()),
        )
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

        // Single-select custom REPLACES the selection.
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
    /// The delivered ask ENDS the turn (`terminate`), leaves the card on the
    /// gate, and its tool text tells the model the answer arrives as the
    /// user's next message.
    #[tokio::test]
    async fn ask_delivery_terminates_the_turn_and_registers_the_card() {
        let (gate, _rx) = gate_with_events();
        let tool = AskUserQuestionTool::new(Arc::clone(&gate));
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
        let result = tool
            .execute("ask-1", params, CancellationToken::new(), &ctx)
            .await
            .expect("the ask tool returns a tool result, not a hard error");
        assert!(!result.is_error);
        assert!(result.terminate, "a delivered ask ends the turn");
        let text = result_text(&result);
        assert!(
            text.contains("This turn ends here"),
            "the delivery text names the turn end: {text}"
        );
        let cards = gate.pending_entries();
        assert_eq!(cards.len(), 1, "the card stays open for the answer");
        assert_eq!(cards[0].0, "ask-1");
        assert_eq!(cards[0].1.input["questions"][0]["id"], "shape-q");
    }

    /// The answer seed folds by card id against the delivered input: an
    /// answer for another card settles nothing, single-select custom
    /// replaces the selection.
    #[tokio::test]
    async fn ask_card_routes_the_answer_seed_by_id() {
        let (gate, _rx) = gate_with_events();
        let tool = AskUserQuestionTool::new(Arc::clone(&gate));
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
        tool.execute("ask-1", params, CancellationToken::new(), &ctx)
            .await
            .expect("delivered");
        let meta = gate.withdraw("ask-1").expect("the card is open");
        let seed = answer_seed_payload(
            &meta.input,
            vec![
                AskAnswer::new("frozen-q".into(), vec!["a".into()], None),
                AskAnswer::new("other-card".into(), vec!["x".into()], None),
                AskAnswer::new("frozen-q".into(), vec!["b".into()], Some("free".into())),
            ],
        );
        assert_eq!(
            seed,
            "The user answered your AskUserQuestion card:\n\
             {\"answers\":[{\"id\":\"frozen-q\",\"selected\":[\"a\"]},{\"id\":\"frozen-q\",\"selected\":[],\"custom\":\"free\"}]}",
            "one row per in-card answer, `id` then `selected` then `custom`, \
             single-select custom replacing the selection, foreign ids dropped"
        );
    }

    /// First settle wins: a withdrawn id is gone, and a second withdrawal
    /// reads as unknown. `withdraw_all` drains whatever is still open.
    #[tokio::test]
    async fn withdraw_first_settle_wins_and_all_drains() {
        let (gate, _rx) = gate_with_events();
        gate.register(
            "ask-1",
            PendingAuthMeta {
                tool_name: crate::tools::ASK_USER_QUESTION.to_string(),
                summary: "t".into(),
                input: serde_json::json!({}),
            },
        );
        assert!(gate.withdraw("ask-1").is_some(), "first settle wins");
        assert!(gate.withdraw("ask-1").is_none(), "already settled");
        gate.register(
            "a",
            PendingAuthMeta {
                tool_name: crate::tools::ASK_USER_QUESTION.to_string(),
                summary: "t".into(),
                input: serde_json::json!({}),
            },
        );
        gate.register(
            "b",
            PendingAuthMeta {
                tool_name: crate::tools::ASK_USER_QUESTION.to_string(),
                summary: "t".into(),
                input: serde_json::json!({}),
            },
        );
        let drained = gate.withdraw_all();
        assert_eq!(drained.len(), 2, "every open card drains");
        assert!(gate.pending_entries().is_empty());
    }

    #[tokio::test]
    async fn ask_from_delegated_gate_is_rejected_as_delegated_caller() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gate = Arc::new(UserQuestionGate::new(tx).with_delegated(true));
        let tool = AskUserQuestionTool::new(Arc::clone(&gate));
        let ctx = tool_ctx();
        let result = tool
            .execute("ask-1", ask_params(), CancellationToken::new(), &ctx)
            .await
            .expect("returns a tool result, not a hard error");
        assert!(result.is_error, "a delegated call is an error to the model");
        let text = result_text(&result);
        assert!(
            text.contains("[DELEGATED_CALLER]"),
            "names the taxonomy code: {text}"
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
    /// them (nor of the seed payload an answer produces) without a conscious
    /// update to the literals below.
    #[test]
    fn ask_surface_bytes_are_frozen() {
        let (gate, _rx) = gate_with_events();
        let tool = AskUserQuestionTool::new(gate);
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
             Calling this tool ENDS your turn: the card is the last thing you emit, \
             and the user's answers arrive as their next message. "
        );
        assert_eq!(
            serde_json::to_string(&tool.parameters_schema()).unwrap(),
            r##"{"type":"object","properties":{"questions":{"type":"array","description":"The questions to ask the user (any number). Each becomes one step in the question drawer.","minItems":1,"items":{"type":"object","properties":{"id":{"type":"string","description":"Stable identifier the user's answer routes by. Optional: one is minted when omitted."},"question":{"type":"string","description":"The full question text to display."},"detail":{"type":"string","description":"Optional markdown support text rendered beneath the question (required when `intent.kind` is set)."},"intent":{"type":"object","description":"Optional interaction intent. `kind` names the specialised surface (e.g. \"plan-review\"); `approve` must be one of this question's option labels — the option that means \"yes\" on that surface.","properties":{"kind":{"type":"string","description":"The intent kind, e.g. \"plan-review\"."},"approve":{"type":"string","description":"The option label that carries the approval verdict."}}},"header":{"type":"string","description":"Short label for the question (max 12 characters)."},"options":{"type":"array","description":"Choices for the user to select from (any number; omit for a free-text-only question).","items":{"type":"object","properties":{"label":{"type":"string","description":"Concise label for the choice (1–5 words)."},"description":{"type":"string","description":"Explanation of what the choice means or implies."},"recommended":{"type":"boolean","description":"Whether this option is the recommended default."}},"required":["label","description"]}},"multiSelect":{"type":"boolean","description":"When true, the user may select multiple options; otherwise exactly one."}},"required":["question","header","multiSelect"]}}},"required":["questions"]}"##
        );
    }

    /// W1 byte-freeze: the delivery text the tool hands back and the
    /// delegated refusal, byte for byte.
    #[tokio::test]
    async fn ask_result_bytes_are_frozen() {
        let (gate, _rx) = gate_with_events();
        let tool = AskUserQuestionTool::new(Arc::clone(&gate));
        let ctx = tool_ctx();
        let delivered = tool
            .execute("ask-1", ask_params(), CancellationToken::new(), &ctx)
            .await
            .expect("the ask tool returns a tool result");
        assert_eq!(
            result_text(&delivered),
            "Question delivered to the user. This turn ends here — the card is the \
             conversation's last item. The user's answers arrive as their next message \
             (a closed card means they will speak instead); do not repeat or answer \
             the questions yourself."
        );

        let (tx, _rx) = mpsc::unbounded_channel();
        let delegated_gate = Arc::new(UserQuestionGate::new(tx).with_delegated(true));
        let delegated = AskUserQuestionTool::new(delegated_gate);
        let refused = delegated
            .execute("ask-1", ask_params(), CancellationToken::new(), &ctx)
            .await
            .expect("a delegated refusal is a tool result, not a hard error");
        assert_eq!(
            result_text(&refused),
            "[DELEGATED_CALLER] A delegated subagent cannot ask the user questions. \
             Include the open question in your final summary so the parent agent — \
             which can ask — resolves it."
        );
    }
}
