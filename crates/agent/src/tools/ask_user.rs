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
    /// 1–4 questions to ask the user. Each becomes one row in the question card.
    questions: Vec<Question>,
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct Question {
    /// The full question text to display.
    question: String,
    /// Short label for the question (max 12 characters).
    header: String,
    /// 2–4 choices for the user to select from.
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
}

impl AgentTool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "AskUserQuestion"
    }

    fn description(&self) -> &str {
        "Ask the user clarifying questions when multiple valid approaches exist \
         and the answer changes what you do next. Use only for decisions that are \
         genuinely the user's to make — not for facts you can verify yourself. \
         Each call carries 1–4 questions, each with 2–4 options; the user may \
         also type a free-form 'Other' answer. Do not use this tool to ask for \
         plan approval or to confirm obvious defaults."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema::<AskUserQuestionInput>()
    }

    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn run(
        &self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        // Unreachable in normal flow: the thread intercepts AskUserQuestion at
        // the authorization gate and builds the result from the UI response.
        cx.background_spawn(async {
            Err("AskUserQuestion is resolved by the UI, not executed".to_string())
        })
    }
}
