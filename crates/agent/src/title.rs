//! Host-private Title agent and its evidence shaping.
//!
//! Title generation is a manox capability, not a Pi parity surface. Each run
//! creates an ephemeral `pi::Agent` with exactly one terminating `Title` tool.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi::agent::Agent;
use pi::coding_agent::ModelRuntime;
use pi::tool::{AgentTool, AgentToolResult, LocalToolContext, ToolContext, ToolError, ToolState};
use pi::types::{AgentMessage, ContentBlock, Model as PiModel, StopReason, StreamOptions};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::goal::{GoalStatus, ThreadGoal};

pub const TITLE_MAX_CHARS: usize = 40;
pub const MESSAGE_SAMPLE_CHARS: usize = 800;
pub const PLAN_SAMPLE_CHARS: usize = 16_000;
const TITLE_TIMEOUT: Duration = Duration::from_secs(20);
const OMITTED: &str = "\n\n[... omitted for safety ...]\n\n";
const SYSTEM_PROMPT: &str = include_str!("title_agent_prompt.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleWakeReason {
    FirstResponse,
    ThirdResponse,
    Stop,
    ToolTerminate,
    PlanStarted,
    GoalStarted,
}

#[derive(Debug, Default)]
pub struct TitleWakeState {
    provider_responses: usize,
    suppress_run_stop: bool,
    milestone_this_turn: bool,
}

impl TitleWakeState {
    pub fn restore(provider_responses: usize, suppress_run_stop: bool) -> Self {
        Self {
            provider_responses,
            suppress_run_stop,
            milestone_this_turn: false,
        }
    }

    pub fn persisted(&self) -> (usize, bool) {
        (self.provider_responses, self.suppress_run_stop)
    }

    pub fn turn_start(&mut self) {
        self.milestone_this_turn = false;
    }

    pub fn assistant_response(&mut self, stop_reason: Option<StopReason>) -> Vec<TitleWakeReason> {
        if matches!(stop_reason, Some(StopReason::Error | StopReason::Aborted)) {
            return Vec::new();
        }
        self.provider_responses += 1;
        let milestone = match self.provider_responses {
            1 => Some(TitleWakeReason::FirstResponse),
            3 => Some(TitleWakeReason::ThirdResponse),
            _ => None,
        };
        self.milestone_this_turn = milestone.is_some();
        let stopped = stop_reason == Some(StopReason::Stop);
        let suppress = stopped && self.suppress_run_stop;
        if stopped {
            self.suppress_run_stop = false;
        }
        milestone
            .or_else(|| (stopped && !suppress).then_some(TitleWakeReason::Stop))
            .into_iter()
            .collect()
    }

    pub fn tool_terminated(&self) -> Option<TitleWakeReason> {
        (!self.milestone_this_turn).then_some(TitleWakeReason::ToolTerminate)
    }

    pub fn start_execution(&mut self) {
        self.suppress_run_stop = true;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleSource {
    Goal(String),
    Plan(String),
    Conversation(Vec<TitleTurn>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleTurn {
    pub role: TitleRole,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleRole {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct TitleRequest {
    pub source: TitleSource,
    pub current_title: Option<String>,
    pub reason: TitleWakeReason,
}

pub fn initial_title(text: &str) -> Option<String> {
    let collapsed = collapse_whitespace(text);
    (!collapsed.is_empty()).then(|| truncate_with_ellipsis(&collapsed, TITLE_MAX_CHARS))
}

pub fn valid_title(raw: &str) -> Option<String> {
    let title = raw.trim();
    if title.is_empty()
        || title.chars().count() > TITLE_MAX_CHARS
        || title.contains(['\n', '\r'])
        || title.starts_with('#')
        || title.contains("**")
        || title.starts_with('`')
        || title.ends_with('`')
    {
        return None;
    }
    Some(title.to_string())
}

pub fn select_source(
    goal: Option<&ThreadGoal>,
    plan_file: Option<&str>,
    messages: &[AgentMessage],
) -> TitleSource {
    if let Some(goal) = goal.filter(|goal| goal.status != GoalStatus::Complete) {
        return TitleSource::Goal(goal.objective.clone());
    }
    if let Some(plan) = plan_file
        .and_then(|path| std::fs::read_to_string(path).ok())
        .filter(|plan| !plan.trim().is_empty())
    {
        return TitleSource::Plan(truncate_head_tail(&plan, PLAN_SAMPLE_CHARS));
    }
    TitleSource::Conversation(sample_conversation(messages))
}

pub fn sample_conversation(messages: &[AgentMessage]) -> Vec<TitleTurn> {
    let turns: Vec<TitleTurn> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::User { .. } => message_text(message).map(|text| TitleTurn {
                role: TitleRole::User,
                text: truncate_with_ellipsis(&collapse_whitespace(&text), MESSAGE_SAMPLE_CHARS),
            }),
            AgentMessage::Assistant {
                stop_reason,
                error_message,
                ..
            } if !matches!(stop_reason, Some(StopReason::Error | StopReason::Aborted))
                && error_message.is_none() =>
            {
                message_text(message).map(|text| TitleTurn {
                    role: TitleRole::Assistant,
                    text: truncate_with_ellipsis(&collapse_whitespace(&text), MESSAGE_SAMPLE_CHARS),
                })
            }
            _ => None,
        })
        .collect();
    if turns.len() <= 6 {
        return turns;
    }
    turns[..3]
        .iter()
        .chain(turns[turns.len() - 3..].iter())
        .cloned()
        .collect()
}

pub fn request_prompt(request: &TitleRequest) -> String {
    let current = request.current_title.as_deref().unwrap_or("(none)");
    let evidence = match &request.source {
        TitleSource::Goal(objective) => format!("SOURCE: ACTIVE GOAL\n{objective}"),
        TitleSource::Plan(markdown) => format!("SOURCE: REVIEWED PLAN\n{markdown}"),
        TitleSource::Conversation(turns) => {
            let transcript = turns
                .iter()
                .map(|turn| {
                    let role = match turn.role {
                        TitleRole::User => "USER",
                        TitleRole::Assistant => "ASSISTANT",
                    };
                    format!("{role}: {}", turn.text)
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            format!("SOURCE: CONVERSATION SAMPLE\n{transcript}")
        }
    };
    format!(
        "Wake reason: {:?}\nCurrent display title (metadata only; never topic evidence): {current}\n\n<untrusted-evidence>\n{evidence}\n</untrusted-evidence>",
        request.reason
    )
}

pub async fn run_title_agent(
    runtime: &ModelRuntime,
    session_model: &PiModel,
    cwd: &Path,
    request: &TitleRequest,
) -> anyhow::Result<Option<String>> {
    let policy = crate::settings::side_calls().title_policy();
    let model = crate::pi_providers::resolve_side_call_model(&policy)
        .unwrap_or_else(|| session_model.clone());
    let stream = (runtime.resolver())(&model)?;
    let result = Arc::new(Mutex::new(None));
    let calls = Arc::new(Mutex::new(0usize));
    let tools = title_tools(Arc::clone(&result), Arc::clone(&calls));
    let env: Arc<dyn pi::env::ExecutionEnv> =
        Arc::new(pi::env::TokioExecutionEnv::new(cwd.to_path_buf()));
    let context: Arc<dyn ToolContext> = Arc::new(LocalToolContext::new(
        env,
        cwd.to_path_buf(),
        Arc::new(ToolState::new()),
    ));
    let mut agent = Agent::new(SYSTEM_PROMPT, model, stream, context).with_tools(tools);
    agent.set_stream_resolver(runtime.resolver());
    agent.set_thinking_level(
        crate::settings::side_call_effort(
            &policy,
            crate::language_model::RequestReasoningEffort::Low,
        )
        .map(|effort| effort.wire_value().to_string()),
    );
    agent.set_stream_options(StreamOptions {
        temperature: Some(0.2),
        max_tokens: crate::settings::side_call_output_cap(policy).map(|n| n as usize),
        timeout: Some(TITLE_TIMEOUT),
        ..Default::default()
    });

    tokio::time::timeout(TITLE_TIMEOUT, async {
        agent.prompt(&request_prompt(request)).await?;
        if result.lock().unwrap().is_none() {
            agent
                .prompt(
                    "Repair the previous response. Call Title exactly once with a valid title; do not answer with prose.",
                )
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("Title agent timed out"))??;
    let title = result.lock().unwrap().clone();
    Ok(title)
}

struct TitleTool {
    result: Arc<Mutex<Option<String>>>,
    calls: Arc<Mutex<usize>>,
}

fn title_tools(
    result: Arc<Mutex<Option<String>>>,
    calls: Arc<Mutex<usize>>,
) -> Arc<[Arc<dyn AgentTool>]> {
    Arc::from(vec![
        Arc::new(TitleTool { result, calls }) as Arc<dyn AgentTool>
    ])
}

#[async_trait::async_trait]
impl AgentTool for TitleTool {
    fn name(&self) -> &str {
        "Title"
    }

    fn description(&self) -> &str {
        "Submit the final concise thread title and terminate the Title agent."
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": TITLE_MAX_CHARS,
                    "description": "A single-line title with no Markdown, at most 40 Unicode characters"
                }
            },
            "required": ["title"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls > 2 || self.result.lock().unwrap().is_some() {
            let mut response = AgentToolResult::error("Title has already been submitted.");
            response.terminate = true;
            return Ok(response);
        }
        let valid = params
            .as_object()
            .filter(|object| object.len() == 1)
            .and_then(|object| object.get("title"))
            .and_then(JsonValue::as_str)
            .and_then(valid_title);
        let mut response = match valid {
            Some(title) => {
                *self.result.lock().unwrap() = Some(title);
                AgentToolResult::text("Title accepted.")
            }
            None => AgentToolResult::error(
                "Invalid title. It must be non-empty, single-line, Markdown-free, and at most 40 Unicode characters.",
            ),
        };
        response.terminate = true;
        Ok(response)
    }
}

fn message_text(message: &AgentMessage) -> Option<String> {
    let content = match message {
        AgentMessage::User { content, .. } | AgentMessage::Assistant { content, .. } => content,
        _ => return None,
    };
    let text = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    format!("{}…", text.chars().take(keep).collect::<String>())
}

fn truncate_head_tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let marker = OMITTED.chars().count();
    let available = max_chars.saturating_sub(marker);
    let head = available / 2;
    let tail = available - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{start}{OMITTED}{end}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::agent_loop::StreamFn;
    use pi::types::{AgentContext, AgentEvent, Usage};
    use tokio::sync::mpsc;

    fn context() -> LocalToolContext {
        LocalToolContext::new(
            Arc::new(pi::env::TokioExecutionEnv::new("/tmp")),
            "/tmp".into(),
            Arc::new(ToolState::new()),
        )
    }

    struct InspectingTitleProvider;

    #[async_trait::async_trait]
    impl StreamFn for InspectingTitleProvider {
        async fn stream(
            &self,
            context: &AgentContext,
            _signal: CancellationToken,
            _events: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            assert_eq!(context.tools.len(), 1);
            assert_eq!(context.tools[0].name(), "Title");
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "title-1".into(),
                    name: "Title".into(),
                    input: serde_json::json!({"title": "专用 Title Agent"}),
                    thought_signature: None,
                }],
                model: "test".into(),
                provider: "test".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason: Some(StopReason::ToolUse),
                raw_stop_reason: None,
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: text.into(),
                signature: None,
            }],
            timestamp: chrono::Utc::now(),
        }
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: text.into(),
                signature: None,
            }],
            model: String::new(),
            provider: String::new(),
            api: String::new(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            raw_stop_reason: None,
            usage: Box::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn assistant_with_blocks(content: Vec<ContentBlock>) -> AgentMessage {
        let mut message = assistant("");
        if let AgentMessage::Assistant {
            content: target, ..
        } = &mut message
        {
            *target = content;
        }
        message
    }

    #[test]
    fn initial_title_collapses_and_caps_unicode() {
        assert_eq!(
            initial_title("  hello\n world ").as_deref(),
            Some("hello world")
        );
        let title = initial_title(&"界".repeat(50)).unwrap();
        assert_eq!(title.chars().count(), 40);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn validation_rejects_multiline_markdown_and_overlong() {
        assert!(valid_title("").is_none());
        assert!(valid_title("a\nb").is_none());
        assert!(valid_title("# heading").is_none());
        assert!(valid_title(&"x".repeat(41)).is_none());
        assert_eq!(valid_title("精炼标题").as_deref(), Some("精炼标题"));
    }

    #[test]
    fn conversation_keeps_first_and_last_three_without_overlap() {
        let messages = (0..8)
            .map(|i| {
                if i % 2 == 0 {
                    user(&format!("u{i}"))
                } else {
                    assistant(&format!("a{i}"))
                }
            })
            .collect::<Vec<_>>();
        let sample = sample_conversation(&messages);
        assert_eq!(sample.len(), 6);
        assert_eq!(sample[0].text, "u0");
        assert_eq!(sample[2].text, "u2");
        assert_eq!(sample[3].text, "a5");
        assert_eq!(sample[5].text, "a7");

        let short = sample_conversation(&messages[..5]);
        assert_eq!(short.len(), 5);
    }

    #[test]
    fn conversation_excludes_images_thinking_tools_and_failed_responses() {
        let image_only = AgentMessage::User {
            content: vec![ContentBlock::Image {
                data: "ignored".into(),
                mime_type: "image/png".into(),
            }],
            timestamp: chrono::Utc::now(),
        };
        let thinking_only = assistant_with_blocks(vec![ContentBlock::Thinking {
            thinking: "private reasoning".into(),
            signature: None,
            redacted: Some(false),
        }]);
        let mut failed = assistant("failed output");
        if let AgentMessage::Assistant { stop_reason, .. } = &mut failed {
            *stop_reason = Some(StopReason::Error);
        }
        let sample = sample_conversation(&[
            image_only,
            user("visible user text"),
            thinking_only,
            failed,
            assistant("visible assistant text"),
        ]);
        assert_eq!(
            sample,
            vec![
                TitleTurn {
                    role: TitleRole::User,
                    text: "visible user text".into(),
                },
                TitleTurn {
                    role: TitleRole::Assistant,
                    text: "visible assistant text".into(),
                },
            ]
        );
    }

    #[test]
    fn plan_truncation_keeps_head_tail_and_marker() {
        let plan = format!("HEAD{}TAIL", "x".repeat(PLAN_SAMPLE_CHARS));
        let truncated = truncate_head_tail(&plan, PLAN_SAMPLE_CHARS);
        assert_eq!(truncated.chars().count(), PLAN_SAMPLE_CHARS);
        assert!(truncated.starts_with("HEAD"));
        assert!(truncated.ends_with("TAIL"));
        assert!(truncated.contains("omitted for safety"));
    }

    #[test]
    fn source_priority_is_goal_then_plan_then_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        std::fs::write(&plan, "# Plan").unwrap();
        let goal = ThreadGoal::new("thread".into(), "Ship Title Agent".into(), None).unwrap();
        assert!(matches!(
            select_source(Some(&goal), plan.to_str(), &[user("chat")]),
            TitleSource::Goal(_)
        ));
        assert!(matches!(
            select_source(None, plan.to_str(), &[user("chat")]),
            TitleSource::Plan(_)
        ));
        assert!(matches!(
            select_source(None, None, &[user("chat")]),
            TitleSource::Conversation(_)
        ));
    }

    #[test]
    fn wake_state_handles_milestones_stop_failures_and_terminate() {
        let mut state = TitleWakeState::default();
        state.turn_start();
        assert_eq!(
            state.assistant_response(Some(StopReason::ToolUse)),
            vec![TitleWakeReason::FirstResponse]
        );
        assert_eq!(state.tool_terminated(), None);
        state.turn_start();
        assert!(state.assistant_response(Some(StopReason::Error)).is_empty());
        assert!(
            state
                .assistant_response(Some(StopReason::Aborted))
                .is_empty()
        );
        assert!(
            state
                .assistant_response(Some(StopReason::ToolUse))
                .is_empty()
        );
        assert_eq!(
            state.tool_terminated(),
            Some(TitleWakeReason::ToolTerminate)
        );
        state.turn_start();
        assert_eq!(
            state.assistant_response(Some(StopReason::Stop)),
            vec![TitleWakeReason::ThirdResponse]
        );
    }

    #[test]
    fn execution_wake_suppresses_only_terminal_stop() {
        let mut state = TitleWakeState::default();
        state.start_execution();
        state.turn_start();
        assert_eq!(
            state.assistant_response(Some(StopReason::Stop)),
            vec![TitleWakeReason::FirstResponse]
        );

        state.start_execution();
        state.turn_start();
        assert!(state.assistant_response(Some(StopReason::Stop)).is_empty());
        state.turn_start();
        assert_eq!(
            state.assistant_response(Some(StopReason::Stop)),
            vec![TitleWakeReason::ThirdResponse]
        );
    }

    #[test]
    fn wake_state_restore_preserves_milestones_and_execution_suppression() {
        let mut state = TitleWakeState::restore(2, true);
        state.turn_start();
        assert_eq!(
            state.assistant_response(Some(StopReason::Stop)),
            vec![TitleWakeReason::ThirdResponse]
        );
        assert_eq!(state.persisted(), (3, false));
    }

    #[test]
    fn title_agent_exposes_only_title_tool() {
        let tools = title_tools(Arc::new(Mutex::new(None)), Arc::new(Mutex::new(0)));
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "Title");
    }

    #[tokio::test]
    async fn provider_context_exposes_only_title_tool() {
        let result = Arc::new(Mutex::new(None));
        let tools = title_tools(Arc::clone(&result), Arc::new(Mutex::new(0)));
        let env: Arc<dyn pi::env::ExecutionEnv> = Arc::new(pi::env::TokioExecutionEnv::new("/tmp"));
        let context: Arc<dyn ToolContext> = Arc::new(LocalToolContext::new(
            env,
            "/tmp".into(),
            Arc::new(ToolState::new()),
        ));
        let model = PiModel {
            provider: "test".into(),
            api: "test".into(),
            id: "test".into(),
            context_window: 8_192,
            max_tokens: 128,
            thinking: pi::types::ThinkingKind::None,
            metadata: Default::default(),
        };
        let mut agent = Agent::new(
            SYSTEM_PROMPT,
            model,
            Arc::new(InspectingTitleProvider),
            context,
        )
        .with_tools(tools);
        agent.prompt("title this").await.unwrap();
        assert_eq!(result.lock().unwrap().as_deref(), Some("专用 Title Agent"));
    }

    #[tokio::test]
    async fn title_tool_allows_one_invalid_call_then_one_repair() {
        let result = Arc::new(Mutex::new(None));
        let tools = title_tools(Arc::clone(&result), Arc::new(Mutex::new(0)));
        let ctx = context();
        let invalid = tools[0]
            .execute(
                "bad",
                serde_json::json!({"title": "# markdown"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(invalid.is_error && invalid.terminate);
        let repaired = tools[0]
            .execute(
                "good",
                serde_json::json!({"title": "Title Agent 状态机"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!repaired.is_error && repaired.terminate);
        assert_eq!(
            result.lock().unwrap().as_deref(),
            Some("Title Agent 状态机")
        );
    }
}
