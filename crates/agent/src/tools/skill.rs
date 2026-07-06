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

pub struct SkillTool;

#[derive(Deserialize, JsonSchema)]
struct SkillInput {
    /// Skill name exactly as it appears in the system prompt's "可用技能" list.
    /// Plugin skills use the `plugin:skill` form (e.g. `gitwork:review`).
    name: String,
}

impl AgentTool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        "查阅指定技能的完整说明书正文。技能名见系统提示「可用技能」列表，\
         插件技能用 `plugin:skill` 形式（如 `gitwork:review`）。\
         仅当任务确实需要该技能的领域知识时调用；不要为已知晓的操作重复查阅。"
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<SkillInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<SkillInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        cx.background_spawn(async move {
            let reg = crate::skill::global();
            match reg.get(&parsed.name) {
                Some(s) => {
                    let mut out = String::new();
                    if !s.description.is_empty() {
                        out.push_str(&s.description);
                        out.push_str("\n\n");
                    }
                    out.push_str(&s.body);
                    Ok(out)
                }
                None => Err(format!(
                    "未知技能: `{}`。请核对系统提示「可用技能」列表中的名称（插件技能需带 `plugin:` 前缀）。",
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
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        self.run(input, cancel, cx)
    }
}
