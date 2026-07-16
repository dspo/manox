//! `AskUserQuestion` tool — surfaces clarifying multiple-choice questions to
//! the user and returns their selections. Modeled after Claude Code's tool of
//! the same name.
//!
//! Unlike other tools, `run` is never reached in normal operation: the tool
//! declares `requires_approval = true`, so `Thread::run_tool` routes it
//! through `ToolCallAuthorization`. The UI renders a question card and sends
//! back a `ToolAuthorizationResponse::AskUserQuestion` with the answers, which
//! the thread short-circuits into a `ToolResult` without invoking `run`. The
//! `run` body here is a defensive fallback for the case where the tool is
//! ever approved without a question-card response.

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
#[allow(dead_code)]
struct AskUserQuestionInput {
    /// 1–3 questions to ask the user. Each becomes one step in the question drawer.
    #[schemars(length(min = 1, max = 3))]
    questions: Vec<Question>,
}

#[derive(Deserialize, JsonSchema)]
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
    multi_select: bool,
}

#[derive(Deserialize, JsonSchema)]
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
        "AskUserQuestion"
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

    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        true
    }

    /// The authorization gate IS this tool's execution: the thread intercepts
    /// the `ToolAuthorizationResponse` and builds the result from the user's
    /// answers, never reaching `run`. YOLO must not bypass it.
    fn requires_user_input(&self) -> bool {
        true
    }

    fn run(
        &self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        // Unreachable in normal flow: the thread intercepts AskUserQuestion at
        // the authorization gate and builds the result from the UI response.
        cx.background_spawn(async {
            Err("AskUserQuestion is resolved by the UI, not executed".to_string())
        })
    }
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
