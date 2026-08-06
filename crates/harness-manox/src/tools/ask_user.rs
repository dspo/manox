//! `AskUserQuestion` tool — surfaces clarifying multiple-choice questions to
//! the user and returns their selections. Modeled after Claude Code's tool of
//! the same name.
//!
//! Unlike other tools, `run` is never reached in normal operation: the tool
//! declares `requires_user_input = true`, so `Thread::run_tool_inner` routes
//! it through a dedicated interactive path (never the approval pipeline —
//! asking a question is read-only and must not be gated by approval modes,
//! reviewer side calls, or the always-allow cache). The UI renders a question
//! card and sends back a `ToolAuthorizationResponse::AskUserQuestion` with the
//! answers, which the thread short-circuits into a `ToolResult` without
//! invoking `run`. The `run` body here is a defensive fallback for the case
//! where the tool is ever invoked outside that interactive path.

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

pub struct AskUserQuestionTool;

// The input structs exist only to drive `JsonSchema` — the agent crate never
// deserializes them (the UI parses the raw `serde_json::Value` directly). Their
// fields are read indirectly via the generated schema, hence `allow(dead_code)`.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub(crate) struct AskUserQuestionInput {
    /// 1–3 questions to ask the user. Each becomes one step in the question drawer.
    #[schemars(length(min = 1, max = 3))]
    questions: Vec<Question>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Question {
    /// The full question text to display.
    question: String,
    /// Short label for the question (max 12 characters).
    header: String,
    /// 2–3 choices for the user to select from.
    #[schemars(length(min = 2, max = 3))]
    options: Vec<QuestionOption>,
    /// When true, the user may select multiple options; otherwise exactly one.
    #[schemars(rename = "multiSelect")]
    #[serde(rename = "multiSelect")]
    multi_select: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct QuestionOption {
    /// Concise label for the choice (1–5 words).
    label: String,
    /// Explanation of what the choice means or implies.
    description: String,
    /// Whether this option is the recommended default.
    #[serde(default)]
    recommended: bool,
}

impl AgentTool for AskUserQuestionTool {
    fn name(&self) -> &str {
        super::ASK_USER_QUESTION
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

    fn input_schema(&self) -> serde_json::Value {
        super::schema::<AskUserQuestionInput>()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    /// Each call asks different questions; a name-keyed "always allow" grant
    /// must never suppress the question card.
    fn is_always_allowable(&self, _input: &serde_json::Value) -> bool {
        false
    }

    /// The interactive path IS this tool's execution: the thread intercepts
    /// the `ToolAuthorizationResponse` and builds the result from the user's
    /// answers, never reaching `run`. Approval modes must not bypass it.
    fn requires_user_input(&self) -> bool {
        true
    }

    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let validation = serde_json::from_value::<AskUserQuestionInput>(input)
            .map_err(|e| format!("Invalid AskUserQuestion input: {e}"))
            .and_then(|input| validate_input(&input));

        // Unreachable in normal flow: the thread intercepts AskUserQuestion at
        // the authorization gate and builds the result from the UI response.
        cx.background_spawn(async move {
            if let Err(e) = validation {
                return Err(e);
            }
            Err("AskUserQuestion is resolved by the UI, not executed".to_string())
        })
    }
}

fn validate_input(input: &AskUserQuestionInput) -> Result<(), String> {
    if !(1..=3).contains(&input.questions.len()) {
        return Err(format!(
            "AskUserQuestion requires 1-3 questions, got {}",
            input.questions.len()
        ));
    }
    for (idx, question) in input.questions.iter().enumerate() {
        if !(2..=3).contains(&question.options.len()) {
            return Err(format!(
                "AskUserQuestion question {} requires 2-3 options, got {}",
                idx + 1,
                question.options.len()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::tool::AgentTool;

    #[test]
    fn schema_declares_question_option_limits_and_recommended_flag() {
        let schema = AskUserQuestionTool.input_schema();
        let questions = find_property(&schema, "questions").expect("questions property");
        assert_eq!(questions.get("minItems"), Some(&json!(1)));
        assert_eq!(questions.get("maxItems"), Some(&json!(3)));

        let options = find_property(&schema, "options").expect("options property");
        assert_eq!(options.get("minItems"), Some(&json!(2)));
        assert_eq!(options.get("maxItems"), Some(&json!(3)));

        let recommended = find_property(&schema, "recommended").expect("recommended property");
        assert_eq!(recommended.get("type"), Some(&json!("boolean")));
    }

    #[test]
    fn ask_user_question_never_enters_the_permission_pipeline() {
        let tool = AskUserQuestionTool;
        let input = serde_json::json!({
            "questions": [{
                "question": "Pick one?",
                "header": "Pick",
                "options": [
                    {"label": "A", "description": ""},
                    {"label": "B", "description": ""}
                ],
                "multiSelect": false
            }]
        });
        // An approval gate, a reviewer side call, or a stale always-allow
        // grant must never suppress the question card.
        assert!(!tool.requires_approval(&input));
        assert!(!tool.is_always_allowable(&input));
        assert!(tool.requires_user_input());
        assert!(tool.is_read_only());
    }

    #[test]
    fn deserialize_accepts_the_schema_declared_multi_select_field() {
        // The JSON schema advertises `multiSelect` to the model; serde must
        // accept that exact name or every schema-conforming call fails
        // validation under `deny_unknown_fields`.
        let raw = serde_json::json!({
            "questions": [{
                "question": "Pick one?",
                "header": "Pick",
                "options": [
                    {"label": "A", "description": ""},
                    {"label": "B", "description": ""}
                ],
                "multiSelect": false
            }]
        });
        let parsed: Result<AskUserQuestionInput, _> = serde_json::from_value(raw);
        assert!(
            parsed.is_ok(),
            "schema-declared multiSelect rejected: {:?}",
            parsed.err()
        );
    }

    #[test]
    fn validate_input_rejects_out_of_range_counts() {
        let valid = AskUserQuestionInput {
            questions: vec![question(2)],
        };
        assert!(validate_input(&valid).is_ok());

        let too_many_questions = AskUserQuestionInput {
            questions: vec![question(2), question(2), question(2), question(2)],
        };
        assert!(validate_input(&too_many_questions).is_err());

        let too_many_options = AskUserQuestionInput {
            questions: vec![question(4)],
        };
        assert!(validate_input(&too_many_options).is_err());
    }

    fn question(option_count: usize) -> Question {
        Question {
            question: "Pick one?".to_string(),
            header: "Pick".to_string(),
            options: (0..option_count)
                .map(|idx| QuestionOption {
                    label: format!("Option {}", idx + 1),
                    description: String::new(),
                    recommended: false,
                })
                .collect(),
            multi_select: false,
        }
    }

    fn find_property<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
        if let Some(property) = value
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get(name))
        {
            return Some(property);
        }

        match value {
            Value::Array(values) => values.iter().find_map(|v| find_property(v, name)),
            Value::Object(map) => map.values().find_map(|v| find_property(v, name)),
            _ => None,
        }
    }
}
