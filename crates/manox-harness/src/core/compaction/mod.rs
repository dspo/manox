// Compaction — context window management through summarization.
//
// When the conversation grows too large for the model's context window,
// compaction generates a structured summary of the oldest messages, keeping
// only the most recent turns intact.

pub mod branch_summarization;

use std::collections::{BTreeSet, HashMap};

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
    /// The messages kept intact across the compaction. An in-memory flow
    /// value only — the session reconstructs the kept segment by walking the
    /// tree from `first_kept_entry_id`.
    pub retained_tail: Vec<AgentMessage>,
    /// Whether the cut split a turn: the history and the discarded turn
    /// prefix were summarized separately.
    pub is_split_turn: bool,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPreparation {
    /// The first entry kept intact; `None` when the whole transcript is
    /// summarized (the wire field is then omitted, matching TS optionality).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub first_kept_entry_id: Option<String>,
    /// The messages replaced by the summary.
    pub messages_to_summarize: Vec<AgentMessage>,
    /// The discarded prefix of a split turn, summarized separately from the
    /// history so the retained suffix keeps its tool chain intact. Empty when
    /// the cut landed on a whole-turn boundary.
    pub turn_prefix_messages: Vec<AgentMessage>,
    /// The messages kept intact after the boundary.
    pub retained_tail: Vec<AgentMessage>,
    /// Whether the cut splits a turn, selecting the dual-summarization path
    /// over the single-call one.
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
        // Count what the model reads, not what was captured: an execution
        // withheld from the context contributes nothing and must not push the
        // estimate toward a compaction it does not cause.
        AgentMessage::BashExecution {
            exclude_from_context: Some(true),
            ..
        } => 0,
        AgentMessage::BashExecution { .. } => {
            crate::core::provider::transform::bash_execution_to_text(message)
                .encode_utf16()
                .count() as u64
        }
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

/// The cut point for a compaction, including whether it splits a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPoint {
    /// Index of the first message retained after compaction.
    pub first_kept_index: usize,
    /// Index of the user message that started the turn the cut lands in;
    /// `None` when the cut is on a whole-turn boundary.
    pub turn_start_index: Option<usize>,
    pub is_split_turn: bool,
}

/// Find the cut point and detect split-turn: when the cut lands inside a
/// turn (the first kept message is not a user message and a user message
/// precedes it), the turn's prefix — from its user message up to the cut — is
/// summarized separately while the suffix stays retained, mirroring TS
/// `findCutPoint`'s `turnStartIndex` / `isSplitTurn`.
pub fn find_cut_point_split(messages: &[AgentMessage], keep_recent_tokens: usize) -> CutPoint {
    let cut = find_cut_point(messages, keep_recent_tokens);
    if cut >= messages.len() || matches!(&messages[cut], AgentMessage::User { .. }) {
        return CutPoint {
            first_kept_index: cut,
            turn_start_index: None,
            is_split_turn: false,
        };
    }
    match messages[..cut]
        .iter()
        .rposition(|m| matches!(m, AgentMessage::User { .. }))
    {
        // A single-message turn (nothing but the user prompt precedes the
        // cut) has no prefix worth summarizing separately.
        Some(start) if start + 1 < cut => CutPoint {
            first_kept_index: cut,
            turn_start_index: Some(start),
            is_split_turn: true,
        },
        _ => CutPoint {
            first_kept_index: cut,
            turn_start_index: None,
            is_split_turn: false,
        },
    }
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

/// Adjust the cut point to the first safe boundary at or after the candidate.
///
/// A boundary is safe when the retained tail both starts on a message a
/// provider accepts as the first request message (anything but a
/// `ToolResult`) and contains no `ToolResult` orphaned by the cut — a result
/// whose `ToolUse` was left behind in the summarized prefix. The tail must
/// never start on a `ToolResult` (its `ToolUse` precedes it), and a cut must
/// not land between a `tool_use` and its result when a `Custom` sits between
/// them: `repair_tool_flow` shows that is a legitimate position, since a
/// `Custom` does not close a tool turn.
///
/// This is position-dependent, not type-dependent. A `Custom` at a turn
/// boundary orphans nothing and is retained verbatim — extension state is
/// not silently discarded into the summary. A `Custom` mid tool chain
/// orphans the trailing result and is advanced past, taking the result and
/// its call together into the prefix.
///
/// TS `findValidCutPoints` lists `custom` as a valid cut and relies on
/// split-turn dual-summarization (`isSplitTurn`) to rescue a mid-turn cut;
/// this Rust compaction does not implement split-turn (see
/// `CompactionPreparation`), so orphaning is prevented at the cut itself.
fn find_safe_cut(messages: &[AgentMessage], candidate: usize) -> usize {
    let tooluse_pos = tooluse_positions(messages);

    let mut idx = candidate.min(messages.len());
    while idx < messages.len() {
        match first_orphaned_result(messages, idx, &tooluse_pos) {
            // A result at or after `idx` whose call precedes `idx` would be
            // orphaned by a cut here. Advance past it so the result and its
            // call land together in the prefix, then re-check.
            Some(orphan) => idx = orphan + 1,
            None => return idx,
        }
    }
    messages.len()
}

/// Map each `ToolUse` id to the position of the `Assistant` carrying it.
fn tooluse_positions(messages: &[AgentMessage]) -> HashMap<&str, usize> {
    let mut map: HashMap<&str, usize> = HashMap::new();
    for (i, msg) in messages.iter().enumerate() {
        if let AgentMessage::Assistant { content, .. } = msg {
            for block in content {
                if let ContentBlock::ToolUse { id, .. } = block {
                    map.insert(id.as_str(), i);
                }
            }
        }
    }
    map
}

/// The first `ToolResult` at or after `idx` whose `ToolUse` precedes `idx` —
/// the result a cut at `idx` would orphan. A result with no matching call is
/// always orphaned (no `ToolUse` exists to retain alongside it).
fn first_orphaned_result(
    messages: &[AgentMessage],
    idx: usize,
    tooluse_pos: &HashMap<&str, usize>,
) -> Option<usize> {
    messages.iter().enumerate().skip(idx).find_map(|(m, msg)| {
        let AgentMessage::ToolResult { tool_call_id, .. } = msg else {
            return None;
        };
        let orphaned = tooluse_pos
            .get(tool_call_id.as_str())
            .is_none_or(|&j| j < idx);
        orphaned.then_some(m)
    })
}

/// Maximum characters of a tool result kept in a serialized summary.
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Truncate for summarization: keep the head and append a marker counting the
/// dropped characters. The limit counts chars (Unicode scalar values — the
/// Rust analogue of the TS string length, which counts UTF-16 code units),
/// never bytes, so a multi-byte char is never split.
fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let Some((end, _)) = text.char_indices().nth(max_chars) else {
        return text.to_string();
    };
    let remaining = text[end..].chars().count();
    format!(
        "{}\n\n[... {} more characters truncated]",
        &text[..end],
        remaining
    )
}

/// Text blocks of a message, joined by newlines — the TS `contentText`.
fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Serialize messages to text for summarization so the model reads them as
/// material rather than a conversation to continue. Custom messages fold to
/// their content as user lines — the TS `convertToLlm` mapping — and tool
/// results are truncated to keep the request within budget.
pub fn serialize_conversation(messages: &[AgentMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        match msg {
            AgentMessage::User { content, .. } | AgentMessage::Custom { content, .. } => {
                let text = content_text(content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            // Summarized as the model saw it; a withheld execution was never
            // part of the conversation being summarized.
            AgentMessage::BashExecution {
                exclude_from_context: Some(true),
                ..
            } => {}
            AgentMessage::BashExecution { .. } => {
                let text = crate::core::provider::transform::bash_execution_to_text(msg);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            AgentMessage::Assistant { content, .. } => {
                let thinking: Vec<&str> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                        _ => None,
                    })
                    .collect();
                let tool_calls: Vec<String> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { name, input, .. } => {
                            let args = input
                                .as_object()
                                .map(|obj| {
                                    obj.iter()
                                        .map(|(k, v)| format!("{k}={v}"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            Some(format!("{name}({args})"))
                        }
                        _ => None,
                    })
                    .collect();
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                if content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { .. }))
                {
                    parts.push(format!("[Assistant]: {}", content_text(content)));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            AgentMessage::ToolResult { content, .. } => {
                let text = content_text(content);
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }
    parts.join("\n\n")
}

/// System prompt for every summarization call: the model produces the
/// structured summary and nothing else.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// Instruction block for a first compaction.
pub const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Instruction block for folding new messages into an existing summary.
pub const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Build a compaction prompt for the LLM.
///
/// The conversation is serialized to text and wrapped in `<conversation>`
/// tags; a previous summary rides in `<previous-summary>` tags and switches
/// the instruction block to the update variant, so iterative compactions
/// extend the checkpoint instead of restarting it.
pub fn build_compaction_prompt(
    compacted_messages: &[AgentMessage],
    existing_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> String {
    let conversation_text = serialize_conversation(compacted_messages);
    let mut prompt = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    let base = match existing_summary {
        Some(summary) => {
            prompt.push_str(&format!(
                "<previous-summary>\n{summary}\n</previous-summary>\n\n"
            ));
            UPDATE_SUMMARIZATION_PROMPT
        }
        None => SUMMARIZATION_PROMPT,
    };
    prompt.push_str(base);
    match custom_instructions {
        Some(ci) if !ci.trim().is_empty() => {
            prompt.push_str(&format!("\n\nAdditional focus: {ci}"));
        }
        _ => {}
    }
    prompt
}

/// The instruction block for summarizing a split turn's prefix — the part of
/// a turn the cut discarded while its suffix stays retained. Mirrors the TS
/// `TURN_PREFIX_SUMMARIZATION_PROMPT`.
pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

/// Build the summarization prompt for a split turn's prefix.
pub fn build_turn_prefix_prompt(prefix_messages: &[AgentMessage]) -> String {
    let conversation_text = serialize_conversation(prefix_messages);
    format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    )
}

/// Build the TS-shaped [`CompactionPreparation`] for the before-compact hook.
///
/// `branch` is the full session path to the root — the same entries TS
/// exposes as `branchEntries`. `messages` is the flat transcript the harness
/// compacts; `cut_point` splits it into `messages_to_summarize` /
/// `retained_tail`. The latest compaction on the path contributes
/// `previous_summary`, and file operations are extracted from the summarized
/// region plus that boundary's recorded file lists.
///
/// Returns `None` when nothing would be summarized — mirroring TS
/// `prepareCompaction` returning `undefined`, which the session layer answers
/// with "Nothing to compact".
pub fn build_preparation(
    branch: &[SessionTreeEntry],
    messages: &[AgentMessage],
    cut_point: &CutPoint,
    first_kept_entry_id: Option<String>,
    tokens_before: u64,
    settings: &CompactionSettings,
) -> Option<CompactionPreparation> {
    // The latest compaction on the path bounds the active context; its
    // summary is the `previousSummary` the summarization folds in. That
    // summary also lives in the transcript as the leading synthetic carrier
    // message, so it is excluded from `messages_to_summarize` — mirroring TS,
    // where `messagesToSummarize` starts at the boundary's first kept entry,
    // not the compaction entry itself. Folding it twice would duplicate the
    // prior summary in the prompt.
    let previous_summary = branch.iter().rev().find_map(|e| match e {
        SessionTreeEntry::Compaction { summary, .. } => Some(summary.clone()),
        _ => None,
    });
    let start = usize::from(previous_summary.is_some());
    let history_end = cut_point
        .turn_start_index
        .unwrap_or(cut_point.first_kept_index)
        .max(start);
    let messages_to_summarize = messages
        .get(start..history_end)
        .map(|s| s.to_vec())
        .unwrap_or_default();
    // A split turn summarizes its prefix (user message up to the cut)
    // separately, so the retained suffix keeps the tool chain intact.
    let turn_prefix_messages = match cut_point.turn_start_index {
        Some(turn_start) => messages
            .get(turn_start..cut_point.first_kept_index)
            .map(|s| s.to_vec())
            .unwrap_or_default(),
        None => Vec::new(),
    };
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }
    let retained_tail = messages[cut_point.first_kept_index..].to_vec();

    let mut file_ops = extract_file_operations(&messages_to_summarize, branch);
    if cut_point.is_split_turn {
        for message in &turn_prefix_messages {
            extract_file_ops_from_message(message, &mut file_ops);
        }
    }

    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings: settings.clone(),
    })
}

/// The transcript holds nothing a compaction would summarize: either the
/// whole conversation fits inside the keep-recent window, or everything
/// beyond it is already folded into the latest boundary's summary. Surfaced
/// by [`crate::harness::AgentHarness::compact`] before any hook or model
/// call, so an overflow recovery can tell "compaction cannot shrink this
/// context" apart from a summarization failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NothingToCompact;

impl std::fmt::Display for NothingToCompact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nothing to compact (session too small)")
    }
}

impl std::error::Error for NothingToCompact {}

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
    // The latest compaction on the path seeds the accumulator with the file
    // lists it recorded. A hook-authored boundary owns its own details shape;
    // only the harness's `{readFiles, modifiedFiles}` payload carries forward.
    if let Some(SessionTreeEntry::Compaction {
        details: Some(d),
        from_hook,
        ..
    }) = branch
        .iter()
        .rev()
        .find(|e| matches!(e, SessionTreeEntry::Compaction { .. }))
        && *from_hook != Some(true)
    {
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
    for msg in messages {
        extract_file_ops_from_message(msg, &mut ops);
    }
    ops
}

/// Classify a message's assistant tool calls into the file-operation sets.
/// Only `read`, `write`, and `edit` calls carrying a `path` argument count.
pub(crate) fn extract_file_ops_from_message(message: &AgentMessage, ops: &mut FileOperations) {
    let AgentMessage::Assistant { content, .. } = message else {
        return;
    };
    for block in content {
        if let ContentBlock::ToolUse { name, input, .. } = block {
            let Some(path) = input.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            match name.as_str() {
                "Read" => {
                    ops.read.insert(path.to_string());
                }
                "Write" => {
                    ops.written.insert(path.to_string());
                }
                "Edit" => {
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

    /// A budget that keeps only the final assistant cuts inside the tool
    /// turn: the split cut point reports the turn start and flags the split,
    /// with the tool chain wholly in the prefix.
    #[test]
    fn find_cut_point_split_detects_mid_turn_cut() {
        let msgs = vec![
            make_user("do the work"),
            make_tool_use_assistant("t1", "Read", "a.rs"),
            make_tool_result("t1", "Read"),
            make_assistant("done"),
        ];
        // keep=1 retains only the final assistant: the cut lands at index 3,
        // inside the turn, with the user message at 0 as the turn start.
        let cp = find_cut_point_split(&msgs, 1);
        assert!(cp.is_split_turn, "{cp:?}");
        assert_eq!(cp.first_kept_index, 3);
        assert_eq!(cp.turn_start_index, Some(0));

        // A whole-turn budget (keep the user prompt too) is not a split.
        let cp = find_cut_point_split(&msgs, 30);
        assert!(!cp.is_split_turn, "{cp:?}");
        assert_eq!(cp.turn_start_index, None);
    }

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

    fn assistant_with_usage(usage: Usage) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
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
                    name: "Read".into(),                     // 4
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
            raw_stop_reason: None,
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
            tool_name: "Read".into(),
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
    fn estimate_tokens_counts_the_bash_projection_not_the_capture() {
        let visible = AgentMessage::BashExecution {
            command: "ls".into(),
            output: "hi".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            exclude_from_context: None,
            timestamp: chrono::Utc::now(),
        };
        let projected = crate::core::provider::transform::bash_execution_to_text(&visible);
        assert_eq!(
            estimate_tokens(&visible),
            (projected.encode_utf16().count() as u64).div_ceil(4)
        );

        // A withheld execution is not in the model's context, so it cannot
        // push the estimate toward a compaction it does not cause.
        let excluded = AgentMessage::BashExecution {
            command: "cat big".into(),
            output: "x".repeat(10_000),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            exclude_from_context: Some(true),
            timestamp: chrono::Utc::now(),
        };
        assert_eq!(estimate_tokens(&excluded), 0);
    }

    #[test]
    fn serialized_conversation_folds_bash_and_omits_withheld() {
        let visible = AgentMessage::BashExecution {
            command: "ls".into(),
            output: "hi".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            exclude_from_context: None,
            timestamp: chrono::Utc::now(),
        };
        let excluded = AgentMessage::BashExecution {
            command: "cat secret".into(),
            output: "token".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            exclude_from_context: Some(true),
            timestamp: chrono::Utc::now(),
        };
        let text = serialize_conversation(&[visible, excluded]);
        assert!(text.contains("[User]: Ran `ls`"), "{text}");
        assert!(!text.contains("token"), "{text}");
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
            raw_stop_reason: None,
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
            make_tool_use_assistant("t1", "Read", "a.rs"),
            make_tool_result("t1", "Read"),
            make_tool_use_assistant("t2", "Edit", "b.rs"),
            make_tool_result("t2", "Edit"),
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

    /// `find_safe_cut` is position-dependent, not type-dependent. A `Custom`
    /// mid tool chain (between a `tool_use` and its result — the shape
    /// `repair_tool_flow` can produce) orphans the trailing result and is
    /// advanced past. A `Custom` at a turn boundary orphans nothing and is
    /// retained verbatim — recent extension state is not discarded into the
    /// summary.
    #[test]
    fn find_safe_cut_retains_safe_custom_skips_orphaning_one() {
        fn custom() -> AgentMessage {
            AgentMessage::Custom {
                custom_type: "note".into(),
                content: vec![ContentBlock::Text {
                    text: "x".into(),
                    signature: None,
                }],
                display: false,
                details: None,
                timestamp: chrono::Utc::now(),
            }
        }

        // Mid-chain Custom between a tool_use and its result: a cut here
        // would orphan the result, so find_safe_cut advances past both the
        // Custom and the result to the trailing user.
        let mid_chain = vec![
            make_tool_use_assistant("c1", "Read", "a.rs"),
            custom(),
            make_tool_result("c1", "Read"),
            make_user("next"),
        ];
        assert_eq!(find_safe_cut(&mid_chain, 1), 3);

        // Turn-boundary Custom: no result after it has its call before it,
        // so cutting here orphans nothing — the Custom is retained, not
        // summarized away.
        let at_boundary = vec![
            make_user("q"),
            make_assistant("a"),
            custom(),
            make_user("q2"),
        ];
        assert_eq!(find_safe_cut(&at_boundary, 2), 2);
    }

    #[test]
    fn test_build_compaction_prompt() {
        let msgs = vec![
            make_user("Write a hello world program"),
            make_assistant("I'll create a main.rs file with a hello world program."),
        ];
        let prompt = build_compaction_prompt(&msgs, None, None);
        assert!(prompt.contains("[User]: Write a hello world program"));
        assert!(prompt.contains("main.rs"));
        assert!(prompt.contains(SUMMARIZATION_PROMPT));
        // First compaction has no previous-summary block and no update rules.
        assert!(!prompt.contains("<previous-summary>"));
        assert!(!prompt.contains(UPDATE_SUMMARIZATION_PROMPT));
        // No custom instructions → no focus line.
        assert!(!prompt.contains("Additional focus"));
    }

    #[test]
    fn test_serialize_conversation_covers_every_message_shape() {
        let long_result = "r".repeat(2100);
        let msgs = vec![
            make_user("plain question"),
            AgentMessage::Custom {
                custom_type: "notice".into(),
                content: vec![ContentBlock::Text {
                    text: "custom payload".into(),
                    signature: None,
                }],
                display: false,
                details: None,
                timestamp: chrono::Utc::now(),
            },
            AgentMessage::Assistant {
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "weighing options".into(),
                        signature: None,
                        redacted: None,
                    },
                    ContentBlock::Text {
                        text: "answer".into(),
                        signature: None,
                    },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"path": "a.rs", "offset": 3}),
                        thought_signature: None,
                    },
                ],
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
            },
            AgentMessage::ToolResult {
                tool_call_id: "t1".into(),
                tool_name: "Read".into(),
                content: vec![ContentBlock::Text {
                    text: long_result.clone(),
                    signature: None,
                }],
                is_error: false,
                details: None,
                usage: None,
                added_tool_names: None,
                timestamp: chrono::Utc::now(),
            },
        ];
        let text = serialize_conversation(&msgs);
        assert!(text.contains("[User]: plain question"));
        // A custom message folds to its content as a user line.
        assert!(text.contains("[User]: custom payload"));
        assert!(text.contains("[Assistant thinking]: weighing options"));
        assert!(text.contains("[Assistant]: answer"));
        // json! literal order (TS Object.entries insertion order):
        assert!(text.contains("[Assistant tool calls]: Read(path=\"a.rs\", offset=3)"));
        // Tool results survive, truncated to the budget with a drop marker.
        assert!(text.contains(&format!("[Tool result]: {}", "r".repeat(2000))));
        assert!(text.contains("[... 100 more characters truncated]"));
    }

    #[test]
    fn truncate_for_summary_counts_chars_never_splits_a_multibyte_char() {
        // 2100 chars of 3 bytes each: a byte-indexed cut at 2000 would land
        // inside a char and panic; the char-indexed cut keeps 2000 whole
        // chars and counts the remaining 100.
        let text = "中".repeat(2100);
        let truncated = truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS);
        assert!(truncated.starts_with(&"中".repeat(2000)));
        assert!(truncated.ends_with("[... 100 more characters truncated]"));

        // Exactly at the limit: no truncation, no marker.
        let exact = "界".repeat(TOOL_RESULT_MAX_CHARS);
        assert_eq!(truncate_for_summary(&exact, TOOL_RESULT_MAX_CHARS), exact);

        // An astral char (4 bytes, one scalar value) counts as one char.
        let emoji = "🦀".repeat(2100);
        let truncated = truncate_for_summary(&emoji, TOOL_RESULT_MAX_CHARS);
        assert!(truncated.ends_with("[... 100 more characters truncated]"));
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
            prompt
                .contains("<previous-summary>\nprior session covered the API\n</previous-summary>"),
            "the previous summary text is embedded verbatim: {prompt}"
        );
        assert!(
            prompt.contains(UPDATE_SUMMARIZATION_PROMPT),
            "a previous summary switches the instructions to the update variant: {prompt}"
        );
        assert!(!prompt.contains(SUMMARIZATION_PROMPT));
        // Absent previous summary leaves no stale summary block.
        let prompt = build_compaction_prompt(&msgs, None, None);
        assert!(!prompt.contains("<previous-summary>"));
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
