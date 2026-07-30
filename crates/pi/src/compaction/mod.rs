// Compaction — context window management through summarization.
//
// When the conversation grows too large for the model's context window,
// compaction generates a structured summary of the oldest messages, keeping
// only the most recent turns intact.

pub mod branch_summarization;

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::session::SessionTreeEntry;
use crate::types::{AgentMessage, ContentBlock, StopReason, Usage};

/// Compaction settings. Serializes as camelCase to match the TS Pi
/// `settings.json` on-disk shape (`reserveTokens`, `keepRecentTokens`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    /// Token usage reported by the summarization call, or by a hook override.
    pub usage: Option<Usage>,
    /// Structured payload attached to the boundary (e.g. by a hook override).
    pub details: Option<JsonValue>,
    /// The messages kept intact across the compaction, stored verbatim.
    pub retained_tail: Vec<AgentMessage>,
}

/// File paths touched by the compacted region, grouped by operation kind.
/// Mirrors the TS `FileOperations`; the sets serialize as JSON arrays.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileOperations {
    /// Files inspected by `read` tool calls.
    pub read: BTreeSet<String>,
    /// Files produced by `write` tool calls.
    pub written: BTreeSet<String>,
    /// Files changed by `edit` tool calls.
    pub edited: BTreeSet<String>,
}

/// The compaction preparation handed to the `session_before_compact` hook:
/// the exact messages being summarized and kept, plus the surrounding context
/// the summarization folds in (previous summary, file operations, settings).
///
/// This is a *partial* mirror of the TS `CompactionPreparation`: split-turn
/// compaction is not implemented (the Rust cut always lands on a whole-turn
/// boundary), so `turn_prefix_messages` is always empty and `is_split_turn` is
/// always false. A single turn that exceeds the keep-recent window is kept
/// intact rather than split-and-merged; closing that gap is tracked separately
/// — half-implementing the cut without the dual summarization would drop the
/// prefix messages, so the field stays structurally present but inert.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPreparation {
    /// The first entry kept intact; `None` when the whole transcript is
    /// summarized (the wire field is then omitted, matching TS optionality).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub first_kept_entry_id: Option<String>,
    /// The messages replaced by the summary.
    pub messages_to_summarize: Vec<AgentMessage>,
    /// Messages prefixing a split turn's retained suffix — always empty here.
    pub turn_prefix_messages: Vec<AgentMessage>,
    /// The messages kept intact after the boundary.
    pub retained_tail: Vec<AgentMessage>,
    /// Whether the cut splits an in-progress turn — always false here.
    pub is_split_turn: bool,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// The previous compaction's summary, when this branch already compacted.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub previous_summary: Option<String>,
    /// File paths touched across the summarized messages.
    pub file_ops: FileOperations,
    /// The settings governing this compaction.
    pub settings: CompactionSettings,
}

/// Total context tokens for one usage block: the provider-reported total
/// when present, otherwise the sum of all token classes.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input_tokens
            + usage.output_tokens
            + usage.cache_read_input_tokens
            + usage.cache_creation_input_tokens
    }
}

/// Usage of the most recent completed assistant message, if any.
///
/// A message without a stop reason (aborted, errored, or never finished
/// streaming) carries no trustworthy usage, and a zero-usage block anchors
/// nothing — both are skipped.
pub fn last_assistant_usage(messages: &[AgentMessage]) -> Option<&Usage> {
    messages.iter().rev().find_map(|m| match m {
        AgentMessage::Assistant {
            stop_reason: Some(r),
            usage,
            ..
        } if !matches!(r, StopReason::Error | StopReason::Aborted)
            && calculate_context_tokens(usage) > 0 =>
        {
            Some(&**usage)
        }
        _ => None,
    })
}

/// Estimated context-token usage for a message list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    /// Estimated total context tokens.
    pub tokens: u64,
    /// Tokens reported by the most recent assistant usage block.
    pub usage_tokens: u64,
    /// Estimated tokens after the most recent assistant usage block.
    pub trailing_tokens: u64,
    /// Index of the message that provided usage, or `None` when none did.
    pub last_usage_index: Option<usize>,
}

/// Estimate context tokens for a message list, anchoring on the last
/// assistant usage block when one exists: everything up to and including
/// that message is already reflected in its usage, so only the trailing
/// messages need the character heuristic. Without an anchor the whole list
/// is estimated.
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    let anchor = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, m)| match m {
            AgentMessage::Assistant {
                stop_reason: Some(r),
                usage,
                ..
            } if !matches!(r, StopReason::Error | StopReason::Aborted)
                && calculate_context_tokens(usage) > 0 =>
            {
                Some((i, &**usage))
            }
            _ => None,
        });

    let Some((index, usage)) = anchor else {
        let estimated = messages.iter().map(estimate_tokens).sum();
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(usage);
    let trailing_tokens = messages[index + 1..].iter().map(estimate_tokens).sum();
    ContextUsageEstimate {
        tokens: usage_tokens + trailing_tokens,
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(index),
    }
}

/// Whether context usage exceeds the configured compaction threshold.
///
/// The threshold subtraction is signed: a reserve larger than the window
/// must not saturate to zero and silently disable compaction.
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    settings.enabled
        && (context_tokens as i64) > (context_window as i64) - (settings.reserve_tokens as i64)
}

/// Chars an image is assumed to occupy for token estimation.
const ESTIMATED_IMAGE_CHARS: u64 = 4800;

/// Estimate the token count of one message with the character heuristic
/// (4 UTF-16 code units per token, rounded up).
///
/// User, tool-result, and custom messages count their text plus a flat
/// allowance per image. Assistant messages count text, thinking, and tool
/// calls (name plus serialized arguments) — the pieces that actually occupy
/// context.
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    fn chars_of(blocks: &[ContentBlock]) -> u64 {
        blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text, .. } => text.encode_utf16().count() as u64,
                ContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
                _ => 0,
            })
            .sum()
    }

    let chars: u64 = match message {
        AgentMessage::User { content, .. } => chars_of(content),
        AgentMessage::ToolResult { content, .. } => chars_of(content),
        AgentMessage::Custom { content, .. } => chars_of(content),
        AgentMessage::Assistant { content, .. } => content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text, .. } => text.encode_utf16().count() as u64,
                ContentBlock::Thinking { thinking, .. } => thinking.encode_utf16().count() as u64,
                ContentBlock::ToolUse { name, input, .. } => {
                    name.encode_utf16().count() as u64
                        + serde_json::to_string(input)
                            .unwrap_or_default()
                            .encode_utf16()
                            .count() as u64
                }
                _ => 0,
            })
            .sum(),
    };
    chars.div_ceil(4)
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
pub fn find_cut_point(messages: &[AgentMessage], keep_recent_tokens: usize) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let mut accumulated = 0u64;
    let mut cut = messages.len();

    // Walk backwards from the tail.
    for (i, msg) in messages.iter().enumerate().rev() {
        let msg_tokens = estimate_tokens(msg);

        if accumulated + msg_tokens > keep_recent_tokens as u64 && i < messages.len() - 1 {
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
/// A safe boundary is one where the retained tail starts on a message a
/// provider accepts as the first message of a request — a `User` or
/// `Assistant`, never a `ToolResult`. A `ToolResult` first message would be
/// orphaned: its matching `ToolUse` was summarized into the prefix, so the
/// provider sees a result with no preceding call and rejects the sequence.
///
/// Mirroring TS `findValidCutPoints`, the boundary is the first non-`ToolResult`
/// message at or after the candidate. A tool chain therefore stays intact on
/// whichever side of the cut it lands — the whole `Assistant(tool_use) →
/// ToolResult` run is retained together or summarized together, never split.
fn find_safe_cut(messages: &[AgentMessage], candidate: usize) -> usize {
    let mut idx = candidate.min(messages.len());
    while idx < messages.len() {
        match &messages[idx] {
            AgentMessage::ToolResult { .. } => idx += 1,
            _ => return idx,
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
    custom_instructions: Option<&str>,
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
                AgentMessage::User { content, .. } | AgentMessage::Assistant { content, .. } => {
                    content
                        .iter()
                        .filter_map(|b| {
                            if let crate::types::ContentBlock::Text { text, .. } = b {
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

    // Caller-supplied focus is appended after the conversation, mirroring the
    // TS `Additional focus: ${customInstructions}` so the summarization model
    // weights it instead of silently dropping it.
    let additional_focus = match custom_instructions {
        Some(ci) if !ci.trim().is_empty() => format!("\n\nAdditional focus: {ci}"),
        _ => String::new(),
    };

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
        <conversation>\n{messages_text}\n</conversation>{additional_focus}"
    )
}

/// Build the TS-shaped [`CompactionPreparation`] for the before-compact hook.
///
/// `branch` is the session path (last compaction … leaf) — the same entries TS
/// exposes as `branchEntries`. `messages` is the flat transcript the harness
/// compacts; `cut_point` splits it into `messages_to_summarize` / `retained_tail`.
/// The previous compaction's summary (if the path starts at one) becomes
/// `previous_summary`, and file operations are extracted from the summarized
/// region plus any prior non-hook compaction's recorded file lists.
///
/// Split-turn is not implemented (see [`CompactionPreparation`]); the cut stays
/// on a whole-turn boundary, so `turn_prefix_messages` is empty and
/// `is_split_turn` is false.
pub fn build_preparation(
    branch: &[SessionTreeEntry],
    messages: &[AgentMessage],
    cut_point: usize,
    first_kept_entry_id: Option<String>,
    tokens_before: u64,
    settings: &CompactionSettings,
) -> CompactionPreparation {
    // The path starts at the last compaction boundary when one exists; its
    // summary is the `previousSummary` the summarization folds in. That summary
    // also lives in the transcript as the leading synthetic `summary_message`,
    // so it is excluded from `messages_to_summarize` — mirroring TS, where
    // `messagesToSummarize` starts at the boundary's first kept entry, not the
    // compaction entry itself. Folding it twice would duplicate the prior
    // summary in the prompt.
    let previous_summary = match branch.first() {
        Some(SessionTreeEntry::Compaction { summary, .. }) => Some(summary.clone()),
        _ => None,
    };
    let start = usize::from(previous_summary.is_some());
    let end = cut_point.max(start);
    let messages_to_summarize = messages
        .get(start..end)
        .map(|s| s.to_vec())
        .unwrap_or_default();
    let retained_tail = messages[cut_point..].to_vec();

    let file_ops = extract_file_operations(&messages_to_summarize, branch);

    CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages: Vec::new(),
        retained_tail,
        is_split_turn: false,
        tokens_before,
        previous_summary,
        file_ops,
        settings: settings.clone(),
    }
}

/// File paths touched by the compacted region, mirroring the TS
/// `extractFileOperations`: assistant tool calls with a `path` argument are
/// classified as read / written / edited, and a previous (non-hook) compaction
/// carrying `{readFiles, modifiedFiles}` details seeds the accumulator so file
/// operations survive across repeated compactions.
fn extract_file_operations(
    messages: &[AgentMessage],
    branch: &[SessionTreeEntry],
) -> FileOperations {
    let mut ops = FileOperations::default();
    if let Some(SessionTreeEntry::Compaction {
        details: Some(d),
        from_hook,
        ..
    }) = branch.first()
    {
        // A hook-authored boundary owns its own details shape; only the
        // harness's `{readFiles, modifiedFiles}` payload carries forward here.
        if *from_hook != Some(true) {
            if let Some(arr) = d.get("readFiles").and_then(|v| v.as_array()) {
                for f in arr.iter().filter_map(|v| v.as_str()) {
                    ops.read.insert(f.to_string());
                }
            }
            if let Some(arr) = d.get("modifiedFiles").and_then(|v| v.as_array()) {
                for f in arr.iter().filter_map(|v| v.as_str()) {
                    ops.edited.insert(f.to_string());
                }
            }
        }
    }
    for msg in messages {
        extract_file_ops_from_message(msg, &mut ops);
    }
    ops
}

/// Classify a message's assistant tool calls into the file-operation sets.
/// Only `read`, `write`, and `edit` calls carrying a `path` argument count.
fn extract_file_ops_from_message(message: &AgentMessage, ops: &mut FileOperations) {
    let AgentMessage::Assistant { content, .. } = message else {
        return;
    };
    for block in content {
        if let ContentBlock::ToolUse { name, input, .. } = block {
            let Some(path) = input.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            match name.as_str() {
                "read" => {
                    ops.read.insert(path.to_string());
                }
                "write" => {
                    ops.written.insert(path.to_string());
                }
                "edit" => {
                    ops.edited.insert(path.to_string());
                }
                _ => {}
            }
        }
    }
}

/// Compute the sorted read-only and modified file lists from accumulated
/// operations, mirroring the TS `computeFileLists`: modified = edited ∪
/// written; readFiles = read minus modified.
pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: BTreeSet<String> = file_ops.edited.union(&file_ops.written).cloned().collect();
    let read_files: Vec<String> = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(*f))
        .cloned()
        .collect();
    let modified_files: Vec<String> = modified.into_iter().collect();
    (read_files, modified_files)
}

/// Format the file lists as summary metadata tags, mirroring the TS
/// `formatFileOperations`. Returns the empty string when there are no files,
/// so the summary text is unchanged when no tool touched a file.
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
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
            stop_reason: Some(StopReason::Stop),
            usage: Default::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn assistant_with_usage(usage: Usage) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(usage),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn calculate_context_tokens_prefers_reported_total() {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 2,
            total_tokens: 999,
            ..Default::default()
        };
        assert_eq!(calculate_context_tokens(&usage), 999);

        let usage = Usage {
            total_tokens: 0,
            ..usage
        };
        assert_eq!(calculate_context_tokens(&usage), 20);
    }

    #[test]
    fn last_assistant_usage_skips_unfinished_and_zero_usage() {
        let usable = Usage {
            total_tokens: 100,
            ..Default::default()
        };
        let zero = Usage::default();

        // The last assistant has no stop reason (aborted); the one before has
        // zero usage; the anchor is the earlier completed one.
        let mut unfinished = assistant_with_usage(usable.clone());
        if let AgentMessage::Assistant { stop_reason, .. } = &mut unfinished {
            *stop_reason = None;
        }
        let messages = vec![
            assistant_with_usage(usable.clone()),
            assistant_with_usage(zero),
            unfinished,
        ];
        assert_eq!(
            last_assistant_usage(&messages).map(|u| u.total_tokens),
            Some(100)
        );

        // None qualify at all.
        let mut unfinished = assistant_with_usage(usable);
        if let AgentMessage::Assistant { stop_reason, .. } = &mut unfinished {
            *stop_reason = None;
        }
        let messages = vec![make_user("hi"), unfinished];
        assert!(last_assistant_usage(&messages).is_none());
    }

    #[test]
    fn last_assistant_usage_skips_error_and_aborted_anchors() {
        let usable = Usage {
            total_tokens: 100,
            ..Default::default()
        };

        // Error and Aborted terminations carry no trustworthy usage, so a
        // clean Stop block further back must anchor instead.
        let mut errored = assistant_with_usage(usable.clone());
        if let AgentMessage::Assistant { stop_reason, .. } = &mut errored {
            *stop_reason = Some(StopReason::Error);
        }
        let messages = vec![assistant_with_usage(usable.clone()), errored];
        assert_eq!(
            last_assistant_usage(&messages).map(|u| u.total_tokens),
            Some(100)
        );

        // When every terminal assistant is Error/Aborted, there is no anchor.
        let mut aborted = assistant_with_usage(usable);
        if let AgentMessage::Assistant { stop_reason, .. } = &mut aborted {
            *stop_reason = Some(StopReason::Aborted);
        }
        assert!(last_assistant_usage(&[aborted]).is_none());
    }

    #[test]
    fn estimate_context_tokens_anchors_on_last_usage() {
        let usage = Usage {
            total_tokens: 1000,
            ..Default::default()
        };
        let messages = vec![
            make_user("ignored: covered by usage"),
            assistant_with_usage(usage),
            make_user("12345678"), // 8 chars → 2 tokens
        ];
        let estimate = estimate_context_tokens(&messages);
        assert_eq!(estimate.tokens, 1002);
        assert_eq!(estimate.usage_tokens, 1000);
        assert_eq!(estimate.trailing_tokens, 2);
        assert_eq!(estimate.last_usage_index, Some(1));

        // No anchor: everything is estimated and counts as trailing.
        let messages = vec![make_user("12345678"), make_user("1234")];
        let estimate = estimate_context_tokens(&messages);
        assert_eq!(estimate.tokens, 3);
        assert_eq!(estimate.usage_tokens, 0);
        assert_eq!(estimate.trailing_tokens, 3);
        assert_eq!(estimate.last_usage_index, None);
    }

    #[test]
    fn estimate_tokens_counts_by_message_shape() {
        // User text: ceil(5 / 4) = 2.
        assert_eq!(estimate_tokens(&make_user("hello")), 2);
        assert_eq!(estimate_tokens(&make_user("")), 0);

        // An image is a flat 4800 chars → 1200 tokens.
        let image = AgentMessage::User {
            content: vec![ContentBlock::Image {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            }],
            timestamp: chrono::Utc::now(),
        };
        assert_eq!(estimate_tokens(&image), 1200);

        // Assistant: text + thinking + tool call (name + JSON arguments).
        let assistant = AgentMessage::Assistant {
            content: vec![
                ContentBlock::Text {
                    text: "abcd".into(), // 4
                    signature: None,
                },
                ContentBlock::Thinking {
                    thinking: "abcd".into(), // 4
                    signature: None,
                    redacted: None,
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),                     // 4
                    input: serde_json::json!({"path": "x"}), // 12
                    thought_signature: None,
                },
            ],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Default::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        // (4 + 4 + 4 + 12) / 4 = 6
        assert_eq!(estimate_tokens(&assistant), 6);

        // Tool result counts its text.
        let result = AgentMessage::ToolResult {
            tool_call_id: "t1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::Text {
                text: "12345678".into(),
                signature: None,
            }],
            is_error: false,
            details: None,
            usage: None,
            added_tool_names: None,
            timestamp: chrono::Utc::now(),
        };
        assert_eq!(estimate_tokens(&result), 2);
    }

    #[test]
    fn estimate_tokens_counts_utf16_units() {
        // An emoji is one scalar but two UTF-16 code units — the heuristic
        // follows the JS `string.length` semantics.
        assert_eq!(estimate_tokens(&make_user("💥💥")), 1);
    }

    #[test]
    fn should_compact_respects_threshold_and_enabled() {
        let settings = CompactionSettings::default();
        assert!(!should_compact(100_000, 200_000, &settings));
        assert!(should_compact(190_000, 200_000, &settings));

        // Disabled gate.
        let settings = CompactionSettings {
            enabled: false,
            ..settings
        };
        assert!(!should_compact(190_000, 200_000, &settings));

        // A reserve larger than the window keeps the threshold signed.
        let settings = CompactionSettings::default();
        assert!(should_compact(0, 1_000, &settings));
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
        // After cut, the first kept message is a user or assistant — never a
        // tool result, which would orphan its tool_use in the prefix.
        if cut < msgs.len() {
            let first_kept = &msgs[cut];
            assert!(
                matches!(
                    first_kept,
                    AgentMessage::User { .. } | AgentMessage::Assistant { .. }
                ),
                "first kept at {cut} should start a valid request, got {first_kept:?}"
            );
        }
    }

    fn make_tool_use_assistant(tool_id: &str, tool_name: &str, path: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: tool_id.into(),
                name: tool_name.into(),
                input: serde_json::json!({ "path": path }),
                thought_signature: None,
            }],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Default::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn make_tool_result(tool_id: &str, tool_name: &str) -> AgentMessage {
        AgentMessage::ToolResult {
            tool_call_id: tool_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::Text {
                text: "ok".into(),
                signature: None,
            }],
            is_error: false,
            details: None,
            usage: None,
            added_tool_names: None,
            timestamp: chrono::Utc::now(),
        }
    }

    /// A multi-turn tool-call chain must never be split: the retained tail
    /// never starts on a `ToolResult` (its `ToolUse` would be summarized into
    /// the prefix, orphaning the result and producing an invalid provider
    /// request). Mirrors TS `findValidCutPoints`, which excludes tool results
    /// as cut indices. Covers `user → assistant(tool1) → result1 →
    /// assistant(tool2) → result2 → assistant(final)` across budgets that land
    /// the cut inside the tool-call region.
    #[test]
    fn find_cut_point_never_splits_tool_chain() {
        let msgs = vec![
            make_user("do the work"),
            make_tool_use_assistant("t1", "read", "a.rs"),
            make_tool_result("t1", "read"),
            make_tool_use_assistant("t2", "edit", "b.rs"),
            make_tool_result("t2", "edit"),
            make_assistant("done"),
        ];
        // Dense budget sweep — the token estimates are [3,5,1,5,1,1] (sum 16),
        // so the budget-exhaustion boundary walks across every message. The
        // tool-call region is indices 1..=4; the cut must never land between an
        // assistant(tool_use) and its result regardless of where the budget
        // runs out.
        for keep in 0..=24 {
            let cut = find_cut_point(&msgs, keep);
            if cut < msgs.len() {
                assert!(
                    !matches!(msgs[cut], AgentMessage::ToolResult { .. }),
                    "keep={keep}: tail starts on a ToolResult at {cut}, orphaning its tool_use"
                );
            }
            // If a tool result is retained, its tool_use assistant is retained
            // too — the pair is never split across the boundary.
            for (idx, msg) in msgs.iter().enumerate() {
                if matches!(msg, AgentMessage::ToolResult { .. }) && idx >= cut {
                    assert!(
                        cut < idx,
                        "keep={keep}: result at {idx} retained but its assistant at {} was cut into the prefix",
                        idx - 1
                    );
                }
            }
        }
    }

    #[test]
    fn test_build_compaction_prompt() {
        let msgs = vec![
            make_user("Write a hello world program"),
            make_assistant("I'll create a main.rs file with a hello world program."),
        ];
        let prompt = build_compaction_prompt(&msgs, None, None);
        assert!(prompt.contains("Write a hello world program"));
        assert!(prompt.contains("main.rs"));
        assert!(prompt.contains("summarizing a coding agent"));
        // No custom instructions → no focus line.
        assert!(!prompt.contains("Additional focus"));
    }

    #[test]
    fn test_build_compaction_prompt_folds_custom_instructions() {
        let msgs = vec![make_user("discuss the auth module")];
        let prompt = build_compaction_prompt(&msgs, None, Some("prioritize token usage"));
        assert!(
            prompt.contains("Additional focus: prioritize token usage"),
            "custom instructions are appended as a focus line: {prompt}"
        );
        // Whitespace-only instructions are dropped, not emitted as an empty focus.
        let prompt = build_compaction_prompt(&msgs, None, Some("   \n\t"));
        assert!(!prompt.contains("Additional focus"));
    }

    #[test]
    fn test_build_compaction_prompt_folds_previous_summary() {
        let msgs = vec![make_user("continue the work")];
        let prompt = build_compaction_prompt(&msgs, Some("prior session covered the API"), None);
        assert!(
            prompt.contains("Here is a summary of the earlier conversation"),
            "previous summary is surfaced as context: {prompt}"
        );
        assert!(
            prompt.contains("<summary>\nprior session covered the API\n</summary>"),
            "the previous summary text is embedded verbatim: {prompt}"
        );
        // Absent previous summary leaves no stale summary block.
        let prompt = build_compaction_prompt(&msgs, None, None);
        assert!(!prompt.contains("Here is a summary of the earlier conversation"));
    }

    #[test]
    fn test_compute_file_lists_splits_read_and_modified() {
        let mut ops = FileOperations::default();
        ops.read.insert("a.rs".into());
        ops.read.insert("b.rs".into());
        ops.edited.insert("a.rs".into());
        ops.written.insert("c.rs".into());

        let (read, modified) = compute_file_lists(&ops);
        // `a.rs` is both read and edited → modified wins, leaves read-only.
        assert_eq!(read, vec!["b.rs".to_string()]);
        assert_eq!(modified, vec!["a.rs".to_string(), "c.rs".to_string()]);
    }

    #[test]
    fn test_format_file_operations_empty_when_no_files() {
        assert_eq!(format_file_operations(&[], &[]), "");
        let block = format_file_operations(
            &["a.rs".to_string()],
            &["b.rs".to_string(), "c.rs".to_string()],
        );
        assert!(block.starts_with("\n\n"));
        assert!(block.contains("<read-files>\na.rs\n</read-files>"));
        assert!(block.contains("<modified-files>\nb.rs\nc.rs\n</modified-files>"));
    }
}
