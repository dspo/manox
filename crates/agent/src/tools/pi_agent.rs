//! The `pi_agent` tool — run a pi-engine subagent with the caller's provider
//! configuration bridged into the pi runtime.
//!
//! This is the first manox consumer of pi-extensions: the wiring smoke test
//! that proves the pi stack (manifest-defined agents + `SubagentTool`
//! dispatch) can run against manox's own model config. It coexists with the
//! native `agent` tool; only `subagent_type` values registered in the pi
//! agent registry (Explore today) are reachable here.

use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use pi::agent_loop::StreamResolver;
use pi::coding_agent::ModelRuntime;
use pi::ext_point_agent::AgentRegistry;
use pi::tool::AgentTool as PiAgentToolTrait;
use pi::tool::{LocalToolContext, ToolState};
use pi::types::{ContentBlock, Model, ThinkingKind};
use pi_extensions::agents::{SubagentTool, register_defaults};

use crate::language_model::AnyLanguageModel;
use crate::provider::WireApi;
use crate::tool::{AgentTool as AgentToolTrait, ToolContext};

/// The `pi_agent` tool. Stateless: the model and cwd are read off the
/// `ToolContext` snapshot per call.
pub struct PiAgentTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PiAgentInput {
    /// Registered pi agent name; defaults to `Explore`.
    #[serde(default)]
    subagent_type: Option<String>,
    prompt: String,
}

impl AgentToolTrait for PiAgentTool {
    fn name(&self) -> &str {
        "pi_agent"
    }

    fn description(&self) -> &str {
        "Run a subagent on the pi engine — Explore is a read-only codebase \
         investigator that returns conclusions with file:line citations. Pass \
         the task in `prompt`; `subagent_type` defaults to Explore."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "subagent_type": {
                    "type": "string",
                    "description": "Registered pi agent name (default: Explore)"
                },
                "prompt": {
                    "type": "string",
                    "description": "The task for the subagent"
                }
            },
            "required": ["prompt"]
        })
    }

    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn run(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed = serde_json::from_value::<PiAgentInput>(input);
        let cwd = ctx.cwd().to_path_buf();
        let model = ctx.model().cloned();
        cx.background_spawn(async move {
            let parsed = parsed.map_err(|e| format!("input parse failed: {e}"))?;
            let model = model.ok_or_else(|| "no model bound to this thread".to_string())?;

            // Bridge the caller's provider config into the pi runtime: the
            // same credential + endpoint, dispatched by the manox wire.
            let pi_model = map_model(&model)?;
            let runtime = ModelRuntime::new(stream_resolver(&model)?);

            // The pi read-only tool set the Explore definition is restricted
            // to.
            let tools: Vec<Arc<dyn pi::tool::AgentTool>> = vec![
                Arc::new(pi::tools::read::ReadTool),
                Arc::new(pi::tools::grep::GrepTool),
                Arc::new(pi::tools::find::FindTool),
                Arc::new(pi::tools::ls::LsTool),
            ];
            let mut registry = AgentRegistry::new();
            register_defaults(&mut registry);
            let subagent = SubagentTool::new(Arc::new(registry), tools)
                .with_model_runtime(runtime)
                .with_model(pi_model);

            let pi_ctx = LocalToolContext::new(
                Arc::new(pi::env::TokioExecutionEnv::new(cwd.clone())),
                cwd,
                Arc::new(ToolState::new()),
            );
            let result = subagent
                .execute(
                    "pi_agent",
                    serde_json::json!({
                        "subagent_type": parsed.subagent_type.as_deref().unwrap_or("Explore"),
                        "prompt": parsed.prompt,
                    }),
                    cancel,
                    &pi_ctx,
                )
                .await
                .map_err(|e| format!("pi subagent failed: {e}"))?;
            Ok(pi_result_text(&result))
        })
    }
}

/// Map the thread's manox model onto a pi `Model`.
fn map_model(model: &AnyLanguageModel) -> Result<Model, String> {
    let api = match model.wire_api() {
        WireApi::Anthropic => "anthropic",
        WireApi::Completions => "openai_completions",
        WireApi::Responses => "openai_responses",
        WireApi::Unavailable => return Err("model wire is unavailable".to_string()),
    };
    Ok(Model {
        provider: model.provider_id(),
        api: api.to_string(),
        id: model.id(),
        // manox exposes no context-window accessor; use a conservative cap.
        context_window: 200_000,
        max_tokens: model.max_token_count() as usize,
        thinking: if model.supports_thinking() {
            ThinkingKind::Enabled
        } else {
            ThinkingKind::None
        },
        metadata: Default::default(),
    })
}

/// Build a stream resolver that dispatches by the pi model's wire API to the
/// matching pi `StreamFn`, seeded with the caller's credential and endpoint.
fn stream_resolver(model: &AnyLanguageModel) -> Result<StreamResolver, String> {
    let api_key = model.api_key().to_string();
    let base_url = model.base_url().to_string();
    let wire = model.wire_api();
    Ok(Arc::new(
        move |m: &Model| -> Result<Arc<dyn pi::agent_loop::StreamFn>, anyhow::Error> {
            let stream: Arc<dyn pi::agent_loop::StreamFn> = match wire {
                WireApi::Anthropic => Arc::new(
                    pi::AnthropicStreamFn::new(api_key.clone()).with_base_url(base_url.clone()),
                ),
                WireApi::Completions => Arc::new(
                    pi::CompletionsStreamFn::new(api_key.clone()).with_base_url(base_url.clone()),
                ),
                WireApi::Responses => Arc::new(
                    pi::ResponsesStreamFn::new(api_key.clone()).with_base_url(base_url.clone()),
                ),
                WireApi::Unavailable => return Err(anyhow::anyhow!("model wire is unavailable")),
            };
            let _ = m;
            Ok(stream)
        },
    ))
}

/// Concatenate the pi result's text blocks.
fn pi_result_text(result: &pi::tool::AgentToolResult) -> String {
    let mut out = String::new();
    for block in &result.content {
        if let ContentBlock::Text { text, .. } = block {
            out.push_str(text);
            out.push('\n');
        }
    }
    let text = out.trim().to_string();
    if result.is_error && text.is_empty() {
        "(pi subagent failed)".to_string()
    } else {
        text
    }
}
