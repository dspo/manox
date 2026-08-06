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

use pi::coding_agent::ModelRuntime;
use pi::ext_point_agent::AgentRegistry;
use pi::tool::AgentTool as PiAgentToolTrait;
use pi::tool::{LocalToolContext, ToolState};
use pi::types::ContentBlock;
use pi_extensions::agents::{SubagentTool, register_defaults};

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

            // Resolve the caller's model through the shared pi provider
            // registry: cx endpoints register as "{provider}-{wire_api}".
            let registry = agent::pi_providers::global();
            let provider = format!("{}-{}", model.provider_name(), model.wire_api().display());
            let pi_model = registry
                .resolve_model(&provider, &model.api_model_id())
                .ok_or_else(|| {
                    format!("model {:?} not in the pi provider registry", model.id())
                })?;
            let runtime = ModelRuntime::with_provider_registry(registry);

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
