// Branch summarization — summarize a conversation/session-tree branch.
//
// A branch summary captures what was done across a stretch of the session
// tree: the assistant's intent, the files touched, and the open work. The
// abandoned branch's entries are prepared under a token budget (tool results
// skipped, prior branch/compaction summaries folded to their tagged user
// carriers, harness-authored branch summaries seeding the file lists),
// serialized to plain text, and fed to an LLM via the same StreamFn the agent
// loop uses. The result — preamble, model prose, and a file-operation tail —
// is persisted as a `branch_summary` session entry carrying usage and the
// read/modified file lists.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent_loop::StreamFn;
use crate::compaction::{
    FileOperations, SUMMARIZATION_SYSTEM_PROMPT, compute_file_lists, estimate_tokens,
    extract_file_ops_from_message, format_file_operations, serialize_conversation,
};
use crate::session::SessionTreeEntry;
use crate::types::{
    AgentContext, AgentEvent, AgentMessage, ContentBlock, Model, StopReason, Usage,
};

/// Tokens reserved for the summarization prompt and the model's response —
/// the TS `GenerateBranchSummaryOptions.reserveTokens` default.
pub const RESERVE_TOKENS: usize = 16_384;

/// Result of a branch summarization run, mirroring the TS
/// `generateBranchSummary` result.
#[derive(Debug, Clone)]
pub struct BranchSummaryResult {
    /// The full summary text — preamble, model prose, file-operation tail.
    /// `None` when the run was aborted.
    pub summary: Option<String>,
    /// Usage reported by the summarization call, when recorded.
    pub usage: Option<Usage>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    /// The run was aborted before producing a summary.
    pub aborted: bool,
}

/// The prepared summarization input: messages selected under the token
/// budget plus the file lists accumulated along the way.
#[derive(Debug, Clone)]
pub struct BranchPreparation {
    /// Messages selected for summarization, in chronological order.
    pub messages: Vec<AgentMessage>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    /// Estimated tokens of the selected messages.
    pub total_tokens: u64,
}

/// Extract a message from a session entry — the TS `getMessageFromEntry`:
/// tool results are skipped (the assistant tool call carries their context),
/// custom messages fold to their custom form, and branch/compaction summaries
/// become their tagged user carriers.
pub fn get_message_from_entry(entry: &SessionTreeEntry) -> Option<AgentMessage> {
    match entry {
        SessionTreeEntry::Message {
            message: AgentMessage::ToolResult { .. },
            ..
        } => None,
        SessionTreeEntry::Message { message, .. } => Some(message.clone()),
        SessionTreeEntry::CustomMessage {
            custom_type,
            content,
            details,
            display,
            timestamp,
            ..
        } => Some(AgentMessage::Custom {
            custom_type: custom_type.clone(),
            content: content.clone(),
            display: *display,
            details: details.clone(),
            timestamp: *timestamp,
        }),
        SessionTreeEntry::BranchSummary {
            summary, timestamp, ..
        } => Some(crate::session::branch_summary_message(summary, *timestamp)),
        SessionTreeEntry::Compaction {
            summary, timestamp, ..
        } => Some(crate::session::compaction_summary_message(
            summary, *timestamp,
        )),
        _ => None,
    }
}

/// Prepare branch entries for summarization under a token budget — the TS
/// `prepareBranchEntries`. Walks newest to oldest, keeping messages until the
/// budget is exhausted; a compaction/branch-summary carrier past the budget
/// still fits while the accumulated total stays under 90% of it. Harness-
/// authored branch summaries seed the read/modified file lists first so
/// cumulative tracking survives nested navigation.
pub fn prepare_branch_entries(
    entries: &[SessionTreeEntry],
    token_budget: u64,
) -> BranchPreparation {
    let mut ops = FileOperations::default();
    for entry in entries {
        if let SessionTreeEntry::BranchSummary {
            details: Some(details),
            from_hook,
            ..
        } = entry
            && *from_hook != Some(true)
        {
            if let Some(arr) = details.get("readFiles").and_then(|v| v.as_array()) {
                for f in arr.iter().filter_map(|v| v.as_str()) {
                    ops.read.insert(f.to_string());
                }
            }
            if let Some(arr) = details.get("modifiedFiles").and_then(|v| v.as_array()) {
                for f in arr.iter().filter_map(|v| v.as_str()) {
                    ops.edited.insert(f.to_string());
                }
            }
        }
    }

    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut total_tokens: u64 = 0;
    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut ops);
        let tokens = estimate_tokens(&message);
        if token_budget > 0 && total_tokens + tokens > token_budget {
            if matches!(
                entry,
                SessionTreeEntry::Compaction { .. } | SessionTreeEntry::BranchSummary { .. }
            ) && (total_tokens as u128) < (token_budget as u128) * 9 / 10
            {
                messages.insert(0, message);
                total_tokens += tokens;
            }
            break;
        }
        messages.insert(0, message);
        total_tokens += tokens;
    }
    let (read_files, modified_files) = compute_file_lists(&ops);
    BranchPreparation {
        messages,
        read_files,
        modified_files,
        total_tokens,
    }
}

/// The instruction block asking the model for a structured branch summary —
/// the TS `BRANCH_SUMMARY_PROMPT`.
pub const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// The preamble prepended to every model-produced branch summary.
pub const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

/// Build the summarization prompt for a prepared branch — the TS
/// `generateBranchSummary` prompt: the serialized conversation wrapped in
/// `<conversation>` tags followed by the instruction block.
pub fn build_branch_summary_prompt(
    messages: &[AgentMessage],
    custom_instructions: Option<&str>,
    replace_instructions: bool,
) -> String {
    let conversation = serialize_conversation(messages);
    let instructions = match custom_instructions {
        Some(extra) if replace_instructions => extra.to_string(),
        Some(extra) => format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {extra}"),
        None => BRANCH_SUMMARY_PROMPT.to_string(),
    };
    format!("<conversation>\n{conversation}\n</conversation>\n\n{instructions}")
}

/// Summarize an abandoned branch — the TS `generateBranchSummary`. An empty
/// preparation yields the TS "No content to summarize" marker without a model
/// call; a cancelled run reports `aborted`; a failed run propagates the
/// provider error.
// All inputs are distinct semantic surfaces (entries, model, runtime,
// budget, instructions, cancellation, request options); grouping them would
// hide a caller's missing argument behind a struct literal.
#[allow(clippy::too_many_arguments)]
pub async fn summarize_branch(
    entries: &[SessionTreeEntry],
    model: &Model,
    stream_fn: Arc<dyn StreamFn>,
    token_budget: u64,
    custom_instructions: Option<&str>,
    replace_instructions: bool,
    signal: CancellationToken,
    stream_options: &crate::types::StreamOptions,
) -> Result<BranchSummaryResult, anyhow::Error> {
    let preparation = prepare_branch_entries(entries, token_budget);
    if preparation.messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: Some("No content to summarize".to_string()),
            usage: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
            aborted: false,
        });
    }
    let prompt = build_branch_summary_prompt(
        &preparation.messages,
        custom_instructions,
        replace_instructions,
    );
    let context = AgentContext {
        system_prompt: SUMMARIZATION_SYSTEM_PROMPT.to_string(),
        messages: vec![AgentMessage::user(prompt)],
        tools: Arc::from(Vec::new()),
        model: model.clone(),
        thinking_level: None,
        cache_retention: crate::types::CacheRetention::None,
        session_id: None,
        metadata: Default::default(),
        stream_options: stream_options.clone(),
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    // Drain events concurrently with the stream: the channel caps at 64, so a
    // longer stream would deadlock if the receiver only ran after it returned.
    let stream_fn_for_task = Arc::clone(&stream_fn);
    let handle = tokio::spawn(async move { stream_fn_for_task.stream(&context, signal, tx).await });
    while rx.recv().await.is_some() {}
    let response = match handle.await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e),
        Err(join_err) => return Err(anyhow::Error::new(join_err)),
    };

    match &response {
        AgentMessage::Assistant {
            stop_reason: Some(StopReason::Aborted),
            ..
        } => {
            return Ok(BranchSummaryResult {
                summary: None,
                usage: None,
                read_files: Vec::new(),
                modified_files: Vec::new(),
                aborted: true,
            });
        }
        AgentMessage::Assistant {
            stop_reason: Some(StopReason::Error),
            error_message,
            ..
        } => {
            anyhow::bail!(
                "branch summary failed: {}",
                error_message.as_deref().unwrap_or("unknown error")
            );
        }
        _ => {}
    }

    let (text, usage) = match &response {
        AgentMessage::Assistant { content, usage, .. } => (
            content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            (**usage).clone(),
        ),
        _ => (String::new(), Usage::default()),
    };
    let summary = format!(
        "{BRANCH_SUMMARY_PREAMBLE}{text}{}",
        format_file_operations(&preparation.read_files, &preparation.modified_files)
    );
    Ok(BranchSummaryResult {
        summary: Some(summary),
        usage: Some(usage),
        read_files: preparation.read_files,
        modified_files: preparation.modified_files,
        aborted: false,
    })
}

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

    fn message_entry(id: &str, parent: Option<&str>, message: AgentMessage) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.into(),
            parent_id: parent.map(Into::into),
            timestamp: chrono::Utc::now(),
            message,
        }
    }

    /// Tool results are excluded; custom messages fold to their custom form;
    /// branch summaries become their tagged user carriers.
    #[test]
    fn get_message_from_entry_skips_tool_results_and_folds_carriers() {
        let tool_result = message_entry(
            "tr",
            Some("a"),
            AgentMessage::ToolResult {
                tool_call_id: "t1".into(),
                tool_name: "read".into(),
                content: vec![ContentBlock::Text {
                    text: "ok".into(),
                    signature: None,
                }],
                is_error: false,
                details: None,
                usage: None,
                added_tool_names: None,
                timestamp: chrono::Utc::now(),
            },
        );
        assert!(get_message_from_entry(&tool_result).is_none());

        let user = message_entry("u", None, make_user("hello"));
        assert!(matches!(
            get_message_from_entry(&user),
            Some(AgentMessage::User { .. })
        ));

        let custom = SessionTreeEntry::CustomMessage {
            id: "c".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            custom_type: "note".into(),
            content: vec![ContentBlock::Text {
                text: "hi".into(),
                signature: None,
            }],
            details: None,
            display: true,
        };
        assert!(matches!(
            get_message_from_entry(&custom),
            Some(AgentMessage::Custom { custom_type, .. }) if custom_type == "note"
        ));

        let branch = SessionTreeEntry::BranchSummary {
            id: "bs".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            from_id: "root".into(),
            summary: "prior branch".into(),
            details: None,
            usage: None,
            from_hook: Some(false),
        };
        let carrier = get_message_from_entry(&branch).expect("branch summary carrier");
        let AgentMessage::User { content, .. } = &carrier else {
            panic!("carrier is a user message");
        };
        let ContentBlock::Text { text, .. } = &content[0] else {
            panic!("carrier is text");
        };
        assert!(text.contains("prior branch"));
        assert!(text.starts_with(crate::session::BRANCH_SUMMARY_PREFIX));

        let setting = SessionTreeEntry::ModelChange {
            id: "m".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            provider: "p".into(),
            model_id: "m1".into(),
        };
        assert!(get_message_from_entry(&setting).is_none());
    }

    /// The token budget keeps the newest messages; a branch-summary carrier
    /// past the budget still fits under the 90% rule; prior branch-summary
    /// details seed the file lists.
    #[test]
    fn prepare_branch_entries_applies_budget_and_carrier_rule() {
        let entries = vec![
            message_entry("u1", None, make_user("first")),
            message_entry(
                "a1",
                Some("u1"),
                assistant_with_tool_use("write", serde_json::json!({"path": "a.rs"})),
            ),
            message_entry("u2", Some("a1"), make_user("second")),
            message_entry(
                "a2",
                Some("u2"),
                assistant_with_tool_use("read", serde_json::json!({"path": "b.rs"})),
            ),
        ];
        // A budget that admits the newest message but not the pair keeps only
        // the newest (a2: ~5 tokens; u2 adds ~2 more).
        let prep = prepare_branch_entries(&entries, 6);
        assert_eq!(prep.messages.len(), 1);
        assert!(matches!(&prep.messages[0], AgentMessage::Assistant { .. }));
        assert_eq!(prep.total_tokens, estimate_tokens(&prep.messages[0]));

        // Zero budget admits everything, newest first; tool results excluded.
        let with_tool_result = vec![
            entries[0].clone(),
            entries[1].clone(),
            message_entry(
                "tr",
                Some("a1"),
                AgentMessage::ToolResult {
                    tool_call_id: "t1".into(),
                    tool_name: "write".into(),
                    content: vec![ContentBlock::Text {
                        text: "ok".into(),
                        signature: None,
                    }],
                    is_error: false,
                    details: None,
                    usage: None,
                    added_tool_names: None,
                    timestamp: chrono::Utc::now(),
                },
            ),
        ];
        let prep = prepare_branch_entries(&with_tool_result, 0);
        assert_eq!(prep.messages.len(), 2, "tool result excluded");
        // read-only list: b.rs read but not modified; a.rs written => modified.
        let with_second = vec![
            entries[0].clone(),
            entries[1].clone(),
            entries[2].clone(),
            entries[3].clone(),
        ];
        let prep = prepare_branch_entries(&with_second, 0);
        assert_eq!(prep.read_files, vec!["b.rs".to_string()]);
        assert_eq!(prep.modified_files, vec!["a.rs".to_string()]);

        // A prior branch summary seeds the file lists and folds into the
        // carrier under the 90% rule.
        let branch_summary = SessionTreeEntry::BranchSummary {
            id: "bs".into(),
            parent_id: Some("u2".into()),
            timestamp: chrono::Utc::now(),
            from_id: "u2".into(),
            summary: "prior exploration".into(),
            details: Some(serde_json::json!({
                "readFiles": ["old.rs"],
                "modifiedFiles": ["old.rs"],
            })),
            usage: None,
            from_hook: Some(false),
        };
        let entries = vec![entries[0].clone(), branch_summary];
        let prep = prepare_branch_entries(&entries, 0);
        assert_eq!(prep.read_files, Vec::<String>::new());
        assert_eq!(prep.modified_files, vec!["old.rs".to_string()]);
        let has_carrier = prep.messages.iter().any(|m| {
            matches!(
                m,
                AgentMessage::User { content, .. }
                    if content.iter().any(|b| matches!(
                        b,
                        ContentBlock::Text { text, .. } if text.contains("prior exploration")
                    ))
            )
        });
        assert!(
            has_carrier,
            "the branch summary carrier folds into the messages"
        );
    }

    /// The prompt is the TS shape: serialized conversation in `<conversation>`
    /// tags followed by the structured instruction block.
    #[test]
    fn build_prompt_matches_ts_shape() {
        let messages = vec![
            make_user("add a hello world"),
            make_assistant("I will create main.rs"),
        ];
        let prompt = build_branch_summary_prompt(&messages, None, false);
        assert!(prompt.starts_with("<conversation>\n[User]: add a hello world"));
        assert!(prompt.contains("\n</conversation>\n\n"));
        assert!(prompt.ends_with(BRANCH_SUMMARY_PROMPT));

        let replaced = build_branch_summary_prompt(&messages, Some("Focus only on errors."), true);
        assert_eq!(
            replaced,
            format!(
                "<conversation>\n{}\n</conversation>\n\nFocus only on errors.",
                serialize_conversation(&messages)
            )
        );

        let appended = build_branch_summary_prompt(&messages, Some("Focus on errors."), false);
        assert!(appended.ends_with("\n\nAdditional focus: Focus on errors."));
        assert!(appended.contains(BRANCH_SUMMARY_PROMPT));
    }
}
