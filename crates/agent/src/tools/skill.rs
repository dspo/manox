//! The `skill` tool — model-invokable reference document lookup.
//!
//! Skills are passive markdown references (see `agent::skill`). The system
//! prompt advertises each skill's `name` + `description` so the model knows what
//! is available; this tool delivers the full body on demand. The model decides
//! when a task calls for a skill's knowledge — there is no auto-injection of
//! skill bodies, only the summary in the prompt. This mirrors Claude Code's
//! Skill tool.

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, ToolOutputSink};
use crate::tools::schema;

pub struct SkillTool {
    pub lang: crate::language::Language,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillInput {
    /// Skill name exactly as it appears in the system prompt's available-skills list.
    /// Plugin skills use the `plugin:skill` form (e.g. `gitwork:review`).
    name: String,
}

impl AgentTool for SkillTool {
    fn name(&self) -> &str {
        super::SKILL
    }
    fn description(&self) -> &str {
        "Consult the full body of a named skill. Skill names appear in the system prompt's \
         \"Available skills\" list; plugin skills use the `plugin:skill` form (e.g. `gitwork:review`). \
         Call only when the task genuinely needs the skill's domain knowledge; don't re-fetch one \
         you already know."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<SkillInput>()
    }
    /// Read-only: delivers a skill's reference body, never mutates the world.
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed = match serde_json::from_value::<SkillInput>(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move { Err(format!("input parse failed: {e}")) });
            }
        };
        let lang = self.lang;
        cx.background_spawn(async move {
            let reg = crate::skill::global();
            match reg.get(&parsed.name) {
                Some(s) => crate::prompt::render(
                    crate::prompt::PromptTemplate::SkillBody,
                    lang,
                    &crate::prompt::SkillBodyData {
                        description: (!s.description.is_empty()).then(|| s.description.clone()),
                        body: s.body.clone(),
                        arguments: None,
                    },
                )
                .map_err(|e| e.to_string()),
                None => Err(format!(
                    "Unknown skill: `{}`. Check the name against the system prompt's \"Available skills\" list (plugin skills require a `plugin:` prefix).",
                    parsed.name
                )),
            }
        })
    }
    fn run_streaming(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        _sink: ToolOutputSink,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        self.run(input, cancel, ctx, cx)
    }
}
