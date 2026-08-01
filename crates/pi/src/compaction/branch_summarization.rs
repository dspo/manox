// Branch summarization — summarize a conversation/session-tree branch.
//
// A branch summary captures what was done across a stretch of the session
// tree: the assistant's intent, the files touched, and the open work. It is
// produced by feeding the branch's messages (leaf upward, chronologically
// reversed into prompt order) to an LLM via the same StreamFn the agent loop
// uses, and persisted as a `branch_summary` session entry.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent_loop::StreamFn;
use crate::types::{AgentContext, AgentEvent, AgentMessage, ContentBlock, Model};

/// A summary of the work done on a conversation branch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BranchSummary {
    /// Prose summary of the work, produced by the summarizing model.
    pub summary: String,
    /// File paths created, modified, or deleted along the branch.
    pub files_changed: Vec<String>,
}

/// Tool-call field names that carry a filesystem path, in lookup priority.
const PATH_FIELDS: &[&str] = &["path", "file_path", "filePath", "filepath"];

/// Tool names whose `path` argument denotes a file touched by the agent.
fn is_file_tool(name: &str) -> bool {
    matches!(
        name,
        "read"
            | "write"
            | "edit"
            | "str_replace_editor"
            | "create_file"
            | "delete_file"
            | "move_file"
            | "patch"
            | "apply_patch"
            | "multiedit"
    )
}

/// Extract the set of file paths a tool call touched, if any.
fn file_path_of(name: &str, input: &serde_json::Value) -> Option<String> {
    if !is_file_tool(name) {
        return None;
    }
    for field in PATH_FIELDS {
        if let Some(s) = input.get(field).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

/// File paths touched by tool calls anywhere in the branch, in first-seen order.
pub fn extract_files_changed(messages: &[AgentMessage]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for msg in messages {
        let blocks = match msg {
            AgentMessage::Assistant { content, .. } => content,
            _ => continue,
        };
        for block in blocks {
            if let ContentBlock::ToolUse { name, input, .. } = block
                && let Some(path) = file_path_of(name, input)
                && !files.contains(&path)
            {
                files.push(path);
            }
        }
    }
    files
}

/// Render a message list as the prompt body: user/assistant text and tool
/// calls (name plus arguments), omitting tool results and images.
fn render_messages(messages: &[AgentMessage]) -> String {
    messages
        .iter()
        .map(|m| {
            let role = match m {
                AgentMessage::User { .. } => "User",
                AgentMessage::Assistant { .. } => "Assistant",
                AgentMessage::ToolResult { tool_name, .. } => {
                    return format!("Tool result ({tool_name}): (omitted)");
                }
                AgentMessage::Custom { custom_type, .. } => {
                    return format!("Custom ({custom_type}): (omitted)");
                }
            };
            let body = match m {
                AgentMessage::User { content, .. } | AgentMessage::Assistant { content, .. } => {
                    content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text, .. } => Some(text.clone()),
                            ContentBlock::ToolUse { name, input, .. } => {
                                Some(format!("tool: {name} {input}"))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                _ => String::new(),
            };
            format!("{role}: {body}")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Build the prompt asking the model to summarize the branch.
pub fn build_branch_summary_prompt(
    messages: &[AgentMessage],
    files_changed: &[String],
    existing: Option<&BranchSummary>,
) -> String {
    let files_list = files_changed.join("\n");
    let conversation = render_messages(messages);

    let existing_context = match existing {
        Some(e) => format!(
            "An existing summary of this branch is below; update and extend it with the new work.\n\
             <existing_summary>\n{}\n</existing_summary>\n\n",
            e.summary
        ),
        None => String::new(),
    };

    format!(
        "{existing_context}\
        Summarize the work done in the conversation branch below. The summary should be \
        concise (<=300 words) and cover:\n\
        1. The user's main goal or feature being implemented\n\
        2. Key architectural decisions and trade-offs\n\
        3. Notable files created, modified, or deleted (with paths)\n\
        4. Unfinished work or known issues\n\n\
        Do NOT repeat the full conversation. Focus on information essential for continuing \
        the work without losing context.\n\n\
        <files_changed>\n{files_list}\n</files_changed>\n\n\
        <conversation>\n{conversation}\n</conversation>"
    )
}

/// First text block of an assistant message, if it carries any.
fn assistant_text(message: &AgentMessage) -> Option<String> {
    let content = match message {
        AgentMessage::Assistant { content, .. } => content,
        _ => return None,
    };
    let text: String = content
        .iter()
        .filter_map(|b| {
            if let ContentBlock::Text { text, .. } = b {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Summarize a branch by feeding its messages to the LLM via `stream_fn`.
///
/// `messages` is taken in chronological order (oldest first); the function
/// reverses leafward input into prompt order internally. The returned
/// `BranchSummary` carries the model's prose plus the files extracted from
/// tool calls along the branch.
pub async fn summarize_branch(
    messages: &[AgentMessage],
    model: &Model,
    stream_fn: Arc<dyn StreamFn>,
    existing: Option<&BranchSummary>,
) -> Result<BranchSummary, anyhow::Error> {
    let files_changed = extract_files_changed(messages);
    let prompt = build_branch_summary_prompt(messages, &files_changed, existing);

    let context = AgentContext {
        system_prompt: SYSTEM_PROMPT.to_string(),
        messages: vec![AgentMessage::user(prompt)],
        tools: Arc::from(Vec::new()),
        model: model.clone(),
        thinking_level: None,
        cache_retention: Default::default(),
        session_id: None,
        metadata: Default::default(),
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    // Spawn the producer and drain concurrently: the channel caps at 64, so a
    // longer stream would deadlock if the receiver only ran after it returned.
    let stream_fn_for_task = Arc::clone(&stream_fn);
    let handle = tokio::spawn(async move {
        stream_fn_for_task
            .stream(&context, CancellationToken::new(), tx)
            .await
    });
    // The summary text rides on the final assistant message; events are discarded.
    while rx.recv().await.is_some() {}
    let response = match handle.await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e),
        Err(join_err) => return Err(anyhow::Error::new(join_err)),
    };

    let summary = assistant_text(&response).unwrap_or_default();
    Ok(BranchSummary {
        summary,
        files_changed,
    })
}

/// System prompt for the summarizing model.
pub const SYSTEM_PROMPT: &str =
    "You summarize a coding agent's conversation branch into a concise, dense summary.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StopReason;

    fn make_user(text: &str) -> AgentMessage {
        AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                signature: None,
            }],
            timestamp: chrono::Utc::now(),
        }
    }

    fn make_assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                signature: None,
            }],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Default::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn assistant_with_tool_use(name: &str, input: serde_json::Value) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: name.into(),
                input,
                thought_signature: None,
            }],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Default::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn extract_files_changed_dedups_in_first_seen_order() {
        let messages = vec![
            assistant_with_tool_use("write", serde_json::json!({"path": "a.rs"})),
            assistant_with_tool_use("read", serde_json::json!({"path": "b.rs"})),
            // Non-file tool ignored.
            assistant_with_tool_use("grep", serde_json::json!({"pattern": "x"})),
            // Duplicate of a.rs dropped.
            assistant_with_tool_use("edit", serde_json::json!({"path": "a.rs"})),
            // file_path field recognized.
            assistant_with_tool_use("create_file", serde_json::json!({"file_path": "c.rs"})),
        ];
        assert_eq!(
            extract_files_changed(&messages),
            vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()]
        );
    }

    #[test]
    fn build_prompt_includes_files_and_conversation() {
        let messages = vec![
            make_user("add a hello world"),
            make_assistant("I will create main.rs"),
        ];
        let files = vec!["src/main.rs".to_string()];
        let prompt = build_branch_summary_prompt(&messages, &files, None);
        assert!(prompt.contains("src/main.rs"));
        assert!(prompt.contains("add a hello world"));
        assert!(prompt.contains("I will create main.rs"));
        assert!(!prompt.contains("existing_summary"));
    }

    #[test]
    fn build_prompt_extends_existing_summary() {
        let existing = BranchSummary {
            summary: "Prior work".into(),
            files_changed: vec!["a.rs".into()],
        };
        let prompt =
            build_branch_summary_prompt(&[make_user("more")], &["b.rs".into()], Some(&existing));
        assert!(prompt.contains("existing_summary"));
        assert!(prompt.contains("Prior work"));
    }
}
