// The child-side `AskParent` tool: a sub-agent's clarifying question to
// the parent's user.
//
// The ask follows the host's turn-final philosophy — the child never waits
// on a parked result. The delivery ends the child's turn (the run settles
// with whatever it produced), the question surfaces on the parent chat as
// an attributed settlement-class notice, and the answer comes back as a
// CONTINUATION of the same run (`continue_run_id` — the child session and
// every pointer stay intact). The tool result text teaches the child model
// exactly that contract.

use std::sync::Arc;

use crate::tool::{AgentTool as HarnessAgentTool, AgentToolResult, ToolContext, ToolError};
use serde_json::Value;

use crate::ext::subagent::provider::RunObserver;

pub const ASK_PARENT: &str = "AskParent";

const DESCRIPTION: &str = "Ask the parent agent's user one clarifying question when the \
     task's scope or premises are unclear. The delivery ENDS this run's turn — the \
     question surfaces to the user on the parent conversation, and your run pauses. \
     The user's answer arrives as a continuation of this same run with full context; \
     do not guess, do not answer the question yourself.";

const ESCALATED_TEXT: &str = "Question delivered to the parent's user. This turn ends here — \
     the run pauses with everything it has produced. The answer arrives as a \
     continuation of this run; resume work from it when it does.";

pub struct AskParentTool {
    run_id: String,
    observer: Arc<dyn RunObserver>,
}

impl AskParentTool {
    pub fn new(run_id: String, observer: Arc<dyn RunObserver>) -> Self {
        Self { run_id, observer }
    }
}

#[async_trait::async_trait]
impl HarnessAgentTool for AskParentTool {
    fn name(&self) -> &str {
        ASK_PARENT
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "One focused question, framed so the user can answer \
                                    it without seeing the rest of your work.",
                },
            },
            "required": ["question"],
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: tokio_util::sync::CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let question = params
            .get("question")
            .and_then(|v| v.as_str())
            .filter(|q| !q.trim().is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArguments("'question' is required and must be non-empty".into())
            })?
            .to_string();
        self.observer.on_escalate(&self.run_id, &question);
        Ok(AgentToolResult {
            content: vec![crate::types::ContentBlock::Text {
                text: ESCALATED_TEXT.to_string(),
                signature: None,
            }],
            details: Some(serde_json::json!({ "escalated": true })),
            is_error: false,
            usage: None,
            added_tool_names: None,
            // The turn ends at the delivery — the run settles, the question
            // rides the parent chat, the answer continues the run.
            terminate: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn test_context() -> crate::tool::LocalToolContext {
        crate::tool::LocalToolContext::new(
            std::sync::Arc::new(crate::core::env::TokioExecutionEnv::new(
                std::env::temp_dir(),
            )),
            std::env::temp_dir(),
            std::sync::Arc::new(crate::tool::ToolState::default()),
        )
    }

    struct RecordingObserver {
        escalations: Mutex<Vec<(String, String)>>,
    }

    impl RunObserver for RecordingObserver {
        fn on_escalate(&self, run_id: &str, question: &str) {
            self.escalations
                .lock()
                .unwrap()
                .push((run_id.to_string(), question.to_string()));
        }
    }

    #[tokio::test]
    async fn a_delivery_ends_the_turn_and_reaches_the_observer() {
        let observer = Arc::new(RecordingObserver {
            escalations: Mutex::new(Vec::new()),
        });
        let tool = AskParentTool::new("sub-1".into(), observer.clone());
        let result = tool
            .execute(
                "tu_1",
                serde_json::json!({ "question": "Which of the two files is authoritative?" }),
                tokio_util::sync::CancellationToken::new(),
                &test_context(),
            )
            .await
            .unwrap();
        assert!(result.terminate, "an escalated ask ends the child's turn");
        assert!(matches!(
            &result.content[0],
            crate::types::ContentBlock::Text { text, .. }
                if text.contains("continuation of this run")
        ));
        assert_eq!(
            observer.escalations.lock().unwrap().as_slice(),
            [(
                "sub-1".to_string(),
                "Which of the two files is authoritative?".to_string()
            )],
            "the observer sees exactly one escalation"
        );

        // An empty question never reaches the user.
        assert!(
            tool.execute(
                "tu_2",
                serde_json::json!({ "question": "  " }),
                tokio_util::sync::CancellationToken::new(),
                &test_context(),
            )
            .await
            .is_err()
        );
        assert_eq!(observer.escalations.lock().unwrap().len(), 1);
    }
}
