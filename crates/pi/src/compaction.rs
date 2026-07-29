// Compaction — context window management through summarization.
//
// When the conversation grows too large for the model's context window,
// compaction generates a structured summary of the oldest messages, keeping
// only the most recent turns intact.

use serde::{Deserialize, Serialize};

use crate::types::AgentMessage;

/// Compaction settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSettings {
    /// Whether compaction is enabled.
    pub enabled: bool,
    /// Tokens to reserve for the response.
    pub reserve_tokens: usize,
    /// Tokens to keep from the recent conversation tail.
    pub keep_recent_tokens: usize,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        CompactionSettings {
            enabled: true,
            reserve_tokens: 16384,
            keep_recent_tokens: 20000,
        }
    }
}

/// The result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// The generated summary text.
    pub summary: String,
    /// The ID of the first entry that was kept (not compacted).
    pub first_kept_entry_id: Option<String>,
    /// Token count before compaction.
    pub tokens_before: u64,
    /// Token count after compaction.
    pub tokens_after: u64,
}

/// Whether compaction should be triggered.
pub fn should_compact(
    context_tokens: usize,
    context_window: usize,
    settings: &CompactionSettings,
) -> bool {
    settings.enabled && context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

/// Estimate the token count for a message using the character/4 heuristic.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

/// Estimate token count for a list of messages.
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> usize {
    messages
        .iter()
        .map(|m| {
            let serialized = serde_json::to_string(m).unwrap_or_default();
            estimate_tokens(&serialized)
        })
        .sum()
}

/// Find the cut point in the message list for compaction.
///
/// Walks from the tail toward the head, accumulating tokens until the
/// keep_recent_tokens budget is exhausted. The cut point is placed at a
/// safe boundary — after an assistant message (not mid-turn tool-call
/// chain).
///
/// Returns the index of the first message to keep (all messages before
/// this index should be compacted).
pub fn find_cut_point(
    messages: &[AgentMessage],
    keep_recent_tokens: usize,
) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let mut accumulated = 0usize;
    let mut cut = messages.len();

    // Walk backwards from the tail.
    for (i, msg) in messages.iter().enumerate().rev() {
        let msg_tokens = estimate_tokens(
            &serde_json::to_string(msg).unwrap_or_default(),
        );

        if accumulated + msg_tokens > keep_recent_tokens && i < messages.len() - 1 {
            // Budget exhausted. Place the cut after the last safe boundary.
            cut = find_safe_cut(messages, i + 1);
            break;
        }

        accumulated += msg_tokens;
        cut = i;
    }

    // Ensure the cut is at a safe point.
    find_safe_cut(messages, cut)
}

/// Adjust the cut point to a safe boundary.
///
/// A safe boundary is after an assistant or tool-result message,
/// not mid-turn (e.g., after a user message that hasn't been answered).
fn find_safe_cut(messages: &[AgentMessage], candidate: usize) -> usize {
    if candidate >= messages.len() {
        return messages.len();
    }

    // Walk forward from the candidate to find a safe boundary.
    // A safe boundary is after an Assistant or ToolResult message.
    let mut idx = candidate;
    while idx < messages.len() {
        match &messages[idx] {
            AgentMessage::Assistant { .. } => return idx + 1,
            AgentMessage::ToolResult { .. } => {
                // Continue past tool results — they belong to the turn.
                idx += 1;
            }
            AgentMessage::User { .. } => {
                // A user message at the start means we keep from here.
                return idx;
            }
            _ => idx += 1,
        }
    }

    messages.len()
}

/// Build a compaction prompt for the LLM.
///
/// The summary should capture the key decisions, changes, and context
/// from the compacted messages so the model can continue coherently.
pub fn build_compaction_prompt(
    compacted_messages: &[AgentMessage],
    existing_summary: Option<&str>,
) -> String {
    let prefix = if let Some(summary) = existing_summary {
        format!(
            "Here is a summary of the earlier conversation:\n<summary>\n{summary}\n</summary>\n\n"
        )
    } else {
        String::new()
    };

    let messages_text: String = compacted_messages
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
            let content = match m {
                AgentMessage::User { content, .. }
                | AgentMessage::Assistant { content, .. } => {
                    content
                        .iter()
                        .filter_map(|b| {
                            if let crate::types::ContentBlock::Text { text } = b {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                _ => String::new(),
            };
            format!("{role}: {content}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
        "{prefix}\
        You are summarizing a coding agent's conversation history to save context space. \
        Write a concise summary (≤500 words) covering:\n\
        1. The user's main requests and goals\n\
        2. Key decisions, architecture, and trade-offs made\n\
        3. Files modified, created, or deleted (with paths)\n\
        4. Errors encountered and how they were resolved\n\
        5. Any unfinished work or next steps\n\n\
        Do NOT repeat the full conversation. Focus on information that would be \
        essential for continuing the work without losing context.\n\n\
        <conversation>\n{messages_text}\n</conversation>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContentBlock;

    fn make_user(text: &str) -> AgentMessage {
        AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            timestamp: chrono::Utc::now(),
        }
    }

    fn make_assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            model: "test".into(),
            provider: "test".into(),
            stop_reason: Some(crate::types::StopReason::EndTurn),
            usage: Default::default(),
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_should_compact() {
        let settings = CompactionSettings::default();
        assert!(!should_compact(100_000, 200_000, &settings));
        assert!(should_compact(190_000, 200_000, &settings));
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello"), 1);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_find_cut_point_empty() {
        assert_eq!(find_cut_point(&[], 1000), 0);
    }

    #[test]
    fn test_find_cut_point_small_context() {
        let msgs = vec![make_user("hi"), make_assistant("hello")];
        // All messages fit within the budget.
        let cut = find_cut_point(&msgs, 10000);
        assert_eq!(cut, 0); // Everything fits, cut at start.
    }

    #[test]
    fn test_find_cut_point_safe_boundary() {
        // Build: user -> assistant -> user -> assistant
        let msgs = vec![
            make_user("first question"),
            make_assistant("first answer"),
            make_user("second question"),
            make_assistant("second answer"),
        ];
        // Budget only allows ~21 chars (~5 tokens). Should cut at a safe boundary.
        let cut = find_cut_point(&msgs, 5);
        // The cut should be at a safe boundary, not mid-turn.
        assert!(cut > 0, "cut should be > 0, got {cut}");
        // After cut, the first kept message should be a user or assistant.
        if cut < msgs.len() {
            let first_kept = &msgs[cut];
            assert!(
                matches!(first_kept, AgentMessage::User { .. }),
                "first kept at {cut} should be a user message, got {first_kept:?}"
            );
        }
    }

    #[test]
    fn test_build_compaction_prompt() {
        let msgs = vec![
            make_user("Write a hello world program"),
            make_assistant("I'll create a main.rs file with a hello world program."),
        ];
        let prompt = build_compaction_prompt(&msgs, None);
        assert!(prompt.contains("Write a hello world program"));
        assert!(prompt.contains("main.rs"));
        assert!(prompt.contains("summarizing a coding agent"));
    }
}