// Agent dispatch — the `agent` tool that turns a registered agent
// definition into a running subagent.
//
// Definitions are static manifests (see `pi::ext_point_agent`); this module
// only provides the runtime half: resolving `subagent_type` against the
// registry, mounting the definition's tool subset, and collecting the
// subagent's final text.

use std::sync::Arc;

use pi::coding_agent::create_agent_session;
use pi::ext_point_agent::{AgentDef, AgentRegistry};
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi::tools::truncate::{self, TruncateConfig};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

/// Default max bytes for the subagent's returned text.
const DEFAULT_MAX_BYTES: usize = 128 * 1024;
/// Default max lines for the subagent's returned text.
const DEFAULT_MAX_LINES: usize = 2000;

/// The built-in Explore definition, embedded as a manifest.
pub fn explore_agent_def() -> AgentDef {
    AgentDef::parse_md(include_str!("../../agents/explore.md"))
        .expect("built-in Explore manifest must parse")
}

/// Register the built-in agent definitions.
pub fn register_defaults(registry: &mut AgentRegistry) {
    registry.register(explore_agent_def());
}

/// The `agent` tool — invoke an agent definition as a subagent.
pub struct SubagentTool {
    registry: Arc<AgentRegistry>,
    /// Snapshot of the caller's full tool set, used to resolve `def.tools`.
    tools: Vec<Arc<dyn AgentTool>>,
}

impl SubagentTool {
    pub fn new(registry: Arc<AgentRegistry>, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        SubagentTool { registry, tools }
    }
}

#[async_trait::async_trait]
impl AgentTool for SubagentTool {
    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        "Spawn a subagent from a registered agent definition to handle a focused task \
         in isolation. Returns the subagent's final text."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn requires_approval(&self, _params: &JsonValue) -> bool {
        false
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "subagent_type": {
                    "type": "string",
                    "description": "Name of the registered agent definition to invoke (e.g. Explore)"
                },
                "prompt": {
                    "type": "string",
                    "description": "The task for the subagent"
                },
                "description": {
                    "type": "string",
                    "description": "Short description of the task (shown in the UI)"
                }
            },
            "required": ["subagent_type", "prompt"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let subagent_type = params["subagent_type"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("subagent_type is required".into()))?;
        let prompt = params["prompt"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("prompt is required".into()))?;
        let def = self.registry.get(subagent_type).ok_or_else(|| {
            ToolError::InvalidArguments(format!("unknown subagent_type: {subagent_type}"))
        })?;

        let selected = select_tools(&self.tools, def);
        // `def.model` is not applied here: the core is registry-free, so
        // model resolution is the assembly layer's job.
        let mut session = create_agent_session()
            .with_cwd(ctx.cwd())
            .with_system_prompt(def.system_prompt.clone())
            .with_tools(selected)
            .build()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to start subagent: {e}")))?;

        let messages = session
            .prompt(prompt)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("subagent failed: {e}")))?;
        let text = collect_text(&messages);

        let config = TruncateConfig {
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
        };
        let truncated = truncate::truncate(&text, &config);
        let mut out = truncated.content;
        if truncated.was_truncated {
            out.push_str(&format!(
                "\n\n[output truncated: {} lines, {} bytes]",
                truncated.original_lines, truncated.original_bytes
            ));
        }
        Ok(AgentToolResult::text(out))
    }
}

/// Resolve a definition's tool names against the caller's tool snapshot.
/// An empty `tools` list means the full snapshot.
fn select_tools(tools: &[Arc<dyn AgentTool>], def: &AgentDef) -> Vec<Arc<dyn AgentTool>> {
    if def.tools.is_empty() {
        return tools.to_vec();
    }
    tools
        .iter()
        .filter(|t| def.tools.iter().any(|n| n == t.name()))
        .cloned()
        .collect()
}

/// Concatenate the subagent's assistant text blocks, newest last.
fn collect_text(messages: &[pi::types::AgentMessage]) -> String {
    use pi::types::{AgentMessage, ContentBlock};
    let mut out = String::new();
    for message in messages {
        if let AgentMessage::Assistant { content, .. } = message {
            for block in content {
                if let ContentBlock::Text { text, .. } = block {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn explore_manifest_parses() {
        let def = explore_agent_def();
        assert_eq!(def.name, "Explore");
        assert_eq!(def.tools, vec!["read", "grep", "find", "ls"]);
        assert!(def.system_prompt.contains("read-only codebase"));
        assert!(def.description.to_lowercase().contains("read-only"));
    }

    #[test]
    fn select_tools_filters_by_definition() {
        let read = pi::tools::read::ReadTool;
        let write = pi::tools::write::WriteTool;
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(read), Arc::new(write)];
        let def = AgentDef {
            name: "X".into(),
            description: "d".into(),
            tools: vec!["read".into()],
            model: None,
            system_prompt: "p".into(),
        };
        let selected = select_tools(&tools, &def);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name(), "read");
    }

    #[test]
    fn select_tools_empty_means_all() {
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(pi::tools::read::ReadTool)];
        let def = AgentDef {
            name: "X".into(),
            description: "d".into(),
            tools: vec![],
            model: None,
            system_prompt: "p".into(),
        };
        assert_eq!(select_tools(&tools, &def).len(), 1);
    }

    #[test]
    fn subagent_tool_contract() {
        let tool = SubagentTool::new(Arc::new(AgentRegistry::new()), vec![]);
        assert_eq!(tool.name(), "agent");
        assert!(
            tool.parameters_schema()["required"]
                .as_array()
                .unwrap()
                .len()
                == 2
        );
    }

    /// Minimal `ExecutionEnv` standing in for the harness environment.
    struct MockEnv {
        cwd: PathBuf,
    }

    #[async_trait::async_trait]
    impl pi::env::ExecutionEnv for MockEnv {
        fn cwd(&self) -> &Path {
            &self.cwd
        }
        fn join_path(&self, parts: &[&str]) -> PathBuf {
            parts.iter().collect()
        }
        async fn absolute_path(&self, path: &Path) -> Result<PathBuf, pi::env::FileError> {
            Ok(path.to_path_buf())
        }
        async fn read_file(
            &self,
            _path: &Path,
            _offset: Option<usize>,
            _limit: Option<usize>,
        ) -> Result<String, pi::env::FileError> {
            Ok(String::new())
        }
        async fn write_file(&self, _path: &Path, _content: &str) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn exists(&self, _path: &Path) -> Result<bool, pi::env::FileError> {
            Ok(true)
        }
        async fn file_info(&self, _path: &Path) -> Result<pi::env::FileInfo, pi::env::FileError> {
            Ok(pi::env::FileInfo {
                path: _path.to_path_buf(),
                is_dir: false,
                size: 0,
            })
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> Result<Vec<pi::env::FileInfo>, pi::env::FileError> {
            Ok(vec![])
        }
        async fn create_dir(&self, _path: &Path) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn remove(&self, _path: &Path) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn exec(
            &self,
            _command: &str,
            _timeout: std::time::Duration,
            _signal: CancellationToken,
        ) -> Result<pi::env::CommandResult, pi::env::ExecutionError> {
            Ok(pi::env::CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            })
        }
    }

    fn ctx(cwd: &str) -> pi::tool::LocalToolContext {
        pi::tool::LocalToolContext::new(
            Arc::new(MockEnv {
                cwd: PathBuf::from(cwd),
            }),
            PathBuf::from(cwd),
            Arc::new(pi::tool::ToolState::new()),
        )
    }

    #[tokio::test]
    async fn unknown_subagent_type_errors_before_spawning() {
        let tool = SubagentTool::new(Arc::new(AgentRegistry::new()), vec![]);
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"subagent_type": "nope", "prompt": "hi"}),
                CancellationToken::new(),
                &ctx("/tmp"),
            )
            .await;
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("unknown subagent_type: nope"), "got: {err}");
    }

    #[tokio::test]
    async fn missing_arguments_are_rejected() {
        let tool = SubagentTool::new(Arc::new(AgentRegistry::new()), vec![]);
        let result = tool
            .execute(
                "c1",
                serde_json::json!({}),
                CancellationToken::new(),
                &ctx("/tmp"),
            )
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn collect_text_takes_assistant_text_blocks() {
        use pi::types::{AgentMessage, ContentBlock};
        let messages = vec![
            AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "user text".into(),
                    signature: None,
                }],
                timestamp: chrono::Utc::now(),
            },
            AgentMessage::Assistant {
                content: vec![
                    ContentBlock::Text {
                        text: "answer".into(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: " part 2".into(),
                        signature: None,
                    },
                ],
                model: "m".into(),
                provider: "p".into(),
                api: "anthropic".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason: None,
                raw_stop_reason: None,
                usage: Box::default(),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
        ];
        assert_eq!(collect_text(&messages), "answer\n part 2");
    }
}
