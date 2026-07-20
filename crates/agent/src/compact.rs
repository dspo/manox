//! Context compaction — summarize older history into a single handoff message
//! when the context window fills, preserving a byte-budgeted tail of recent
//! user messages verbatim so the active work survives the cut.
//!
//! Design references the consensus across mature coding agents (summarize old +
//! retain recent raw within a token/byte budget, summary as a user-role
//! message, trigger off API-reported token usage). The implementation is
//! manox's own; only the approach is borrowed.
//!
//! Flow:
//!   1. [`auto_compaction_target_ix`] / [`forced_compaction_target_ix`] decide
//!      whether and where to insert a compaction (a `Role::User` message
//!      carrying a [`MessageContent::Compaction`] block).
//!   2. `Thread::stream_compaction` runs a side LLM call over the messages
//!      before the insertion point to produce the summary text.
//!   3. `build_completion_request` assembles the live request as
//!      `[system][retained recent user messages][compaction][messages after]`
//!      via [`retained_user_messages_before`] + [`latest_compaction_ix`].
//!
//! Compaction is an intentional, infrequent prefix-cache bust: the message
//! prefix changes, so the provider's KV cache misses once. That is the
//! unavoidable cost of reclaiming context and is documented at the call site.

use std::collections::HashMap;

use anyhow::Result;
use futures::StreamExt as _;
use gpui::AsyncApp;
use tokio_util::sync::CancellationToken;

use crate::language_model::{
    AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage, MessageContent, Role, TokenUsage,
};
use crate::message::Message;
use crate::thread::model_facing_content;

/// Auto-compaction is only available for models whose context window is at
/// least this large. Below it there is not enough headroom for a compaction
/// pass to be worthwhile, so the thread is left alone and the UI warns instead.
pub const MIN_COMPACTION_CONTEXT_WINDOW: u64 = 80_000;
/// Trigger fraction of the auto-compact window when a model carries an explicit
/// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` env override (Claude Code parity). The
/// override's window replaces `max_token_count`; absent the override, the
/// user's `settings.auto_compact.threshold` (default 0.8) still applies.
pub const CLAUDE_CODE_AUTO_COMPACT_THRESHOLD: f64 = 0.8;

/// Byte budget for the recent user-message tail retained verbatim alongside a
/// compaction summary. ~4 bytes/token ⇒ ~20k tokens of recent user prompts
/// survive the cut. Only `Text` user messages count toward the budget; tool
/// results and prior compactions are never retained raw.
const RETAINED_USER_MESSAGES_BYTE_BUDGET: usize = 80_000;

/// Hard cap on the estimated request body size (in bytes) before
/// auto-compaction fires regardless of the token threshold. Providers reject
/// requests whose serialized body exceeds their limit; this triggers a
/// compaction pass before that happens. 6 MiB is a conservative estimate —
/// the actual wire size is larger than raw text (JSON framing, tool specs,
/// system prompt) so firing at 6 MiB of text keeps the real body well under
/// a typical 10 MiB provider limit.
pub const MAX_REQUEST_BODY_BYTES: usize = 6 * 1024 * 1024;

/// Tokens that count toward the context limit for compaction triggering:
/// input (cache-creation + cache-read + uncached) plus output. Cache-read
/// tokens still occupy KV cache slots, so they count against the window.
pub fn active_tokens(usage: TokenUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.output_tokens)
}

/// Index of the most recent compaction message at or before `end_ix`, if any.
/// A compaction is a `Role::User` message whose content holds a `Compaction`
/// block. Used to locate the live compaction boundary when assembling a
/// request.
pub fn latest_compaction_ix(messages: &[Message], end_ix: usize) -> Option<usize> {
    let end = end_ix.min(messages.len());
    messages[..end]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(ix, m)| is_compaction(m).then_some(ix))
}

/// Whether a message is a compaction (user-role with a `Compaction` block).
fn is_compaction(m: &Message) -> bool {
    m.role == Role::User
        && m.content
            .iter()
            .any(|c| matches!(c, MessageContent::Compaction(_)))
}

/// The most recent `Role::User` message that already has reported usage, keyed
/// by `Message::id` in `per_request`. Scans from the end so a trailing
/// just-submitted prompt (no usage yet) is skipped — the caller sees the last
/// request's real usage, not `None` while the new turn warms up. Auto-compaction
/// and the cockpit budget display share this rule so the trigger and the UI
/// agree on which usage is "current".
pub fn latest_reported_request_usage(
    messages: &[Message],
    per_request: &HashMap<String, TokenUsage>,
) -> Option<(usize, TokenUsage)> {
    messages.iter().enumerate().rev().find_map(|(ix, m)| {
        if m.role != Role::User {
            return None;
        }
        per_request.get(&m.id).copied().map(|u| (ix, u))
    })
}

/// Decide whether an auto-compaction should fire and where to insert it.
///
/// Returns the insertion index when all hold:
/// - auto-compaction is enabled;
/// - the model's input window is at least [`MIN_COMPACTION_CONTEXT_WINDOW`];
/// - a prior user message has reported usage (`per_request` keyed by
///   `Message::id`);
/// - no compaction already sits after that usage point (avoid re-compacting
///   the same region twice);
/// - that usage's [`active_tokens`] meets the threshold (`max_input_tokens *
///   threshold_pct`).
///
/// The insertion index is `len - 1` when the last message is an untracked user
/// prompt (just submitted, no usage yet) so the new prompt stays raw after the
/// compaction; otherwise `len`.
pub fn auto_compaction_target_ix(
    messages: &[Message],
    per_request: &HashMap<String, TokenUsage>,
    auto_compact_enabled: bool,
    max_input_tokens: u64,
    threshold_pct: f64,
) -> Option<usize> {
    if !auto_compact_enabled {
        return None;
    }
    if max_input_tokens < MIN_COMPACTION_CONTEXT_WINDOW {
        return None;
    }
    // Most recent user message that already has reported usage — the last
    // request's token count is the trigger signal. Shared with the cockpit
    // budget display so the UI and the trigger agree on "current" usage.
    let (usage_ix, usage) = latest_reported_request_usage(messages, per_request)?;
    // If a compaction already covers the region past the usage point, the
    // post-compaction tail has not re-filled yet — nothing to do.
    if let Some(c_ix) = latest_compaction_ix(messages, messages.len())
        && c_ix > usage_ix
    {
        return None;
    }
    let threshold = ((max_input_tokens as f64) * threshold_pct).ceil() as u64;
    if active_tokens(usage) < threshold {
        return None;
    }
    // Insert before a trailing untracked user prompt so it survives raw; a
    // tracked tail (assistant, tool result, or a user prompt already sent)
    // compacts at the end.
    let insertion_ix = match messages.last() {
        Some(m) if m.role == Role::User && !per_request.contains_key(&m.id) => {
            messages.len().saturating_sub(1)
        }
        _ => messages.len(),
    };
    Some(insertion_ix)
}

/// Insertion index for a manual `/compact`, or `None` when there is nothing to
/// summarize (no messages, or the thread already ends on a compaction).
pub fn forced_compaction_target_ix(messages: &[Message]) -> Option<usize> {
    match messages.last() {
        None => None,
        Some(m)
            if m.role == Role::User
                && m.content
                    .iter()
                    .any(|b| matches!(b, MessageContent::Compaction(_))) =>
        {
            None
        }
        Some(_) => Some(messages.len()),
    }
}

/// Recent `Text`-only user messages before `compaction_ix`, within the byte
/// budget, oldest-first. The message that overflows the budget is truncated to
/// the remaining budget rather than dropped, so a long final prompt still
/// contributes its head. Tool results and prior compactions are excluded: a
/// raw tool result would orphan from its (now-summarized) tool use, and a raw
/// prior compaction would duplicate the new summary.
pub fn retained_user_messages_before(
    messages: &[Message],
    compaction_ix: usize,
) -> Vec<LanguageModelRequestMessage> {
    let mut remaining_bytes = RETAINED_USER_MESSAGES_BYTE_BUDGET;
    let mut retained: Vec<LanguageModelRequestMessage> = Vec::new();

    for message in messages[..compaction_ix.min(messages.len())].iter().rev() {
        if message.role != Role::User {
            continue;
        }
        // Skip compaction messages and messages carrying tool results; only
        // plain text prompts are safe to retain verbatim.
        if message.content.iter().any(|c| {
            matches!(
                c,
                MessageContent::Compaction(_) | MessageContent::ToolResult(_)
            )
        }) {
            continue;
        }
        if message.content.iter().all(MessageContent::is_empty) {
            continue;
        }
        let request_message = to_request_message(message);
        let byte_count = user_message_byte_len(&request_message);
        if let Some(bytes) = remaining_bytes.checked_sub(byte_count) {
            remaining_bytes = bytes;
            retained.push(request_message);
        } else {
            if remaining_bytes > 0
                && let Some(truncated) = truncate_to_byte_budget(request_message, remaining_bytes)
            {
                retained.push(truncated);
            }
            break;
        }
    }
    retained.reverse();
    retained
}

/// Map a canonical `Message` to its model-facing request form. Applies
/// [`model_facing_content`] per block so agent-tool envelopes are stripped and
/// `Compaction` blocks become text — though retained messages exclude both, so
/// for the retain path this is effectively identity over `Text` blocks.
fn to_request_message(m: &Message) -> LanguageModelRequestMessage {
    LanguageModelRequestMessage {
        role: m.role,
        content: m.content.iter().map(model_facing_content).collect(),
        cache: false,
    }
}

/// Approximate byte length of a user message's text content. `~4 bytes/token`
/// is the budget heuristic; exactness is not required, only a monotone size
/// estimate that bounds the retained tail.
fn user_message_byte_len(msg: &LanguageModelRequestMessage) -> usize {
    msg.content
        .iter()
        .filter_map(|c| c.to_str())
        .map(|s| s.len())
        .sum()
}

/// Estimate the request body size in bytes by summing all messages' text content.
/// This is a rough approximation — the actual wire size includes JSON framing,
/// tool specs, and system prompt overhead, so the real body is larger than
/// this estimate. Used to trigger compaction before hitting provider limits.
pub fn estimate_request_body_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(|c| {
            model_facing_content(c)
                .to_str()
                .map(|s| s.len())
                .unwrap_or(0)
        })
        .sum()
}

/// Truncate a user message's `Text` blocks to fit `budget` bytes (char
/// boundary-safe). Non-text blocks are dropped — they should not appear in the
/// retain path. Returns `None` if no text fits.
fn truncate_to_byte_budget(
    msg: LanguageModelRequestMessage,
    budget: usize,
) -> Option<LanguageModelRequestMessage> {
    let mut remaining = budget;
    let mut content: Vec<MessageContent> = Vec::new();
    for block in msg.content {
        let MessageContent::Text(text) = block else {
            continue;
        };
        if remaining == 0 {
            break;
        }
        let (kept, used) = take_byte_prefix(&text, remaining);
        if used > 0 {
            content.push(MessageContent::Text(kept));
            remaining -= used;
        }
    }
    if content.is_empty() {
        return None;
    }
    Some(LanguageModelRequestMessage {
        role: msg.role,
        content,
        cache: false,
    })
}

/// Take the longest char-boundary-safe prefix of `text` within `budget` bytes.
/// Returns `(prefix, bytes_used)`.
fn take_byte_prefix(text: &str, budget: usize) -> (String, usize) {
    if text.len() <= budget {
        return (text.to_string(), text.len());
    }
    // Floor to a char boundary so we never split a multi-byte sequence.
    let mut cut = budget;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let prefix = &text[..cut];
    (prefix.to_string(), cut)
}

/// Build the side-LLM request that produces a compaction summary. The request
/// is the conversation region before `insertion_ix` (mapped to model-facing
/// form so `agent`-tool envelopes and prior `Compaction` blocks become text),
/// prefixed by the handoff summarization prompt as a system message and tailed
/// by a user turn requesting the summary. Sub-agent nesting is collapsed: the
/// summarizer is a one-shot task, not the agent in agent-mode.
pub fn build_compaction_request(messages: &[Message], insertion_ix: usize) -> LanguageModelRequest {
    let bound = insertion_ix.min(messages.len());
    let mut request_messages: Vec<LanguageModelRequestMessage> = Vec::with_capacity(bound + 2);
    request_messages.push(LanguageModelRequestMessage {
        role: Role::System,
        content: vec![MessageContent::Text(
            crate::prompt::render_static(crate::prompt::PromptTemplate::SideCallCompactSystem)
                .expect("compact system prompt render"),
        )],
        cache: false,
    });
    for m in &messages[..bound] {
        request_messages.push(LanguageModelRequestMessage {
            role: m.role,
            content: m.content.iter().map(model_facing_content).collect(),
            cache: false,
        });
    }
    request_messages.push(LanguageModelRequestMessage {
        role: Role::User,
        content: vec![MessageContent::Text(
            crate::prompt::render_static(
                crate::prompt::PromptTemplate::SideCallCompactFinalInstruction,
            )
            .expect("compact final instruction render"),
        )],
        cache: false,
    });
    let messages = coalesce_same_role(request_messages);
    LanguageModelRequest {
        messages,
        tools: Vec::new(),
        tool_choice: None,
        temperature: Some(0.0),
        thinking_allowed: false,
        reasoning_effort: None,
    }
}

/// Merge runs of consecutive same-role messages by concatenating their content
/// blocks in order. Anthropic's wire rejects adjacent same-role messages;
/// compaction assembles `[retained user...][compaction user][...]` which can
/// produce such runs, so every compaction-shaped request is normalized through
/// this pass before it reaches a provider.
pub fn coalesce_same_role(
    messages: Vec<LanguageModelRequestMessage>,
) -> Vec<LanguageModelRequestMessage> {
    let mut out: Vec<LanguageModelRequestMessage> = Vec::with_capacity(messages.len());
    for m in messages {
        if let Some(last) = out.last_mut()
            && last.role == m.role
        {
            last.content.extend(m.content);
            // A coalesced run is one logical message; keep `cache` as the
            // last segment's flag so a trailing cache anchor survives.
            last.cache = m.cache;
        } else {
            out.push(m);
        }
    }
    out
}

/// Stream a compaction summary from `model` over `request`, draining the
/// response to completion. Returns the accumulated summary text plus the final
/// `TokenUsage` the provider reported (if any) so the caller can attribute the
/// side call's tokens. An empty/whitespace summary is an error: a compaction
/// message with no content is worse than no compaction (it discards history
/// and hands the model nothing). Cancellation yields an error.
pub async fn stream_summary(
    model: &AnyLanguageModel,
    request: LanguageModelRequest,
    cancel: CancellationToken,
    cx: &AsyncApp,
) -> Result<(String, Option<TokenUsage>)> {
    let model = std::sync::Arc::clone(model);
    let call = async move {
        let mut stream = model.stream_completion(request, cx).await?.fuse();
        let mut text = String::new();
        let mut usage: Option<TokenUsage> = None;
        while let Some(event) = stream.next().await {
            let event = event?;
            match event {
                LanguageModelCompletionEvent::Text(delta) => text.push_str(&delta),
                LanguageModelCompletionEvent::UsageUpdate(u) => {
                    // Cumulative for the request; keep the latest (final) snapshot.
                    usage = Some(u);
                }
                LanguageModelCompletionEvent::Stop(_) => break,
                LanguageModelCompletionEvent::Retry { .. }
                | LanguageModelCompletionEvent::ToolUse(_)
                | LanguageModelCompletionEvent::ToolUseJsonParseError { .. }
                | LanguageModelCompletionEvent::Thinking { .. } => {}
            }
        }
        Ok::<_, anyhow::Error>((text, usage))
    };
    let (text, usage) = tokio::select! {
        biased;
        _ = cancel.cancelled() => anyhow::bail!("compaction cancelled"),
        result = call => result?,
    };
    if text.trim().is_empty() {
        anyhow::bail!("compaction produced an empty summary");
    }
    Ok((text, usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::TokenUsage;

    fn user(id: &str, text: &str) -> Message {
        let mut m = Message::user(text.to_string());
        m.id = id.to_string();
        m
    }

    fn compaction(id: &str, summary: &str) -> Message {
        let mut m =
            Message::user_with_content(vec![MessageContent::Compaction(summary.to_string())]);
        m.id = id.to_string();
        m
    }

    #[test]
    fn active_tokens_sums_all_channels() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 20,
        };
        assert_eq!(active_tokens(u), 135);
    }

    #[test]
    fn latest_compaction_ix_finds_most_recent() {
        let msgs = vec![
            user("u1", "hi"),
            compaction("c1", "old"),
            user("u2", "more"),
            compaction("c2", "newer"),
            user("u3", "tail"),
        ];
        assert_eq!(latest_compaction_ix(&msgs, msgs.len()), Some(3));
        assert_eq!(latest_compaction_ix(&msgs, 2), Some(1));
        assert_eq!(latest_compaction_ix(&msgs, 1), None);
    }

    #[test]
    fn auto_target_none_when_disabled() {
        let msgs = vec![user("u1", "hi")];
        let mut per = HashMap::new();
        per.insert("u1".to_string(), huge_usage());
        assert_eq!(
            auto_compaction_target_ix(&msgs, &per, false, 200_000, 0.9),
            None
        );
    }

    #[test]
    fn auto_target_none_below_min_window() {
        let msgs = vec![user("u1", "hi")];
        let mut per = HashMap::new();
        per.insert("u1".to_string(), huge_usage());
        assert_eq!(
            auto_compaction_target_ix(&msgs, &per, true, 10_000, 0.9),
            None
        );
    }

    #[test]
    fn auto_target_none_without_usage() {
        let msgs = vec![user("u1", "hi")];
        assert_eq!(
            auto_compaction_target_ix(&msgs, &HashMap::new(), true, 200_000, 0.9),
            None
        );
    }

    #[test]
    fn auto_target_none_below_threshold() {
        let msgs = vec![user("u1", "hi")];
        let mut per = HashMap::new();
        per.insert(
            "u1".to_string(),
            TokenUsage {
                input_tokens: 1_000,
                ..Default::default()
            },
        );
        // 1_000 < 200_000 * 0.9
        assert_eq!(
            auto_compaction_target_ix(&msgs, &per, true, 200_000, 0.9),
            None
        );
    }

    #[test]
    fn auto_target_inserts_before_untracked_trailing_prompt() {
        // u1 has usage over threshold; u2 is the just-submitted prompt (no
        // usage yet). Compaction inserts before u2 so u2 stays raw.
        let msgs = vec![user("u1", "old"), user("u2", "new")];
        let mut per = HashMap::new();
        per.insert("u1".to_string(), huge_usage());
        assert_eq!(
            auto_compaction_target_ix(&msgs, &per, true, 200_000, 0.9),
            Some(1)
        );
    }

    #[test]
    fn auto_target_appends_when_tail_tracked() {
        // Trailing user message already has usage → compact at the end.
        let msgs = vec![user("u1", "old"), user("u2", "sent")];
        let mut per = HashMap::new();
        per.insert("u1".to_string(), huge_usage());
        per.insert("u2".to_string(), huge_usage());
        assert_eq!(
            auto_compaction_target_ix(&msgs, &per, true, 200_000, 0.9),
            Some(2)
        );
    }
    #[test]
    fn auto_target_fires_at_eighty_pct_claude_code_parity() {
        // CLAUDE_CODE_AUTO_COMPACT_WINDOW parity: at an explicit window W, a
        // compaction fires once active tokens reach 80% of W; below it, no fire.
        let window = 202_745u64;
        let threshold = ((window as f64) * 0.8).ceil() as u64; // 162_196
        let msgs = vec![user("u1", "long session")];
        let mut per = HashMap::new();
        // 79.99…% (one short of the threshold) → no fire.
        per.insert(
            "u1".to_string(),
            TokenUsage {
                input_tokens: threshold - 1,
                ..Default::default()
            },
        );
        assert_eq!(
            auto_compaction_target_ix(&msgs, &per, true, window, 0.8),
            None
        );
        // Exactly 80% → at threshold, fires.
        per.insert(
            "u1".to_string(),
            TokenUsage {
                input_tokens: threshold,
                ..Default::default()
            },
        );
        assert_eq!(
            auto_compaction_target_ix(&msgs, &per, true, window, 0.8),
            Some(1)
        );
    }

    #[test]
    fn auto_target_none_when_compaction_already_past_usage() {
        let msgs = vec![
            user("u1", "old"),
            compaction("c1", "sum"),
            user("u2", "tail"),
        ];
        let mut per = HashMap::new();
        per.insert("u1".to_string(), huge_usage());
        // c1 (ix 1) > usage_ix (0) → already compacted past u1.
        assert_eq!(
            auto_compaction_target_ix(&msgs, &per, true, 200_000, 0.9),
            None
        );
    }

    #[test]
    fn latest_reported_usage_skips_trailing_untracked_prompt() {
        // u1 reported usage; u2 is just-submitted with no usage yet. The helper
        // returns u1, not None — the cockpit keeps showing the last real budget
        // while the new turn warms up.
        let msgs = vec![user("u1", "old"), user("u2", "new")];
        let mut per = HashMap::new();
        per.insert("u1".to_string(), huge_usage());
        let (ix, usage) = latest_reported_request_usage(&msgs, &per).unwrap();
        assert_eq!(ix, 0);
        assert_eq!(usage, huge_usage());
    }

    #[test]
    fn latest_reported_usage_picks_most_recent_tracked() {
        // Multiple user messages all have usage; the last tracked one wins.
        let msgs = vec![user("u1", "a"), user("u2", "b"), user("u3", "c")];
        let mut per = HashMap::new();
        per.insert(
            "u1".to_string(),
            TokenUsage {
                input_tokens: 10_000,
                ..Default::default()
            },
        );
        per.insert("u3".to_string(), huge_usage());
        let (ix, _) = latest_reported_request_usage(&msgs, &per).unwrap();
        assert_eq!(ix, 2);
    }

    #[test]
    fn latest_reported_usage_none_when_no_user_message_tracked() {
        // Only assistant messages or no usage at all → None.
        let msgs = vec![user("u1", "a")];
        assert_eq!(latest_reported_request_usage(&msgs, &HashMap::new()), None);
        let msgs = vec![Message::assistant(vec![])];
        assert_eq!(latest_reported_request_usage(&msgs, &HashMap::new()), None);
    }

    #[test]
    fn latest_reported_usage_ignores_assistant_messages() {
        // A trailing assistant reply must not shadow an earlier tracked user
        // message — assistant messages never carry per-request usage.
        let msgs = vec![
            user("u1", "prompt"),
            Message::assistant(vec![MessageContent::Text("reply".into())]),
        ];
        let mut per = HashMap::new();
        per.insert("u1".to_string(), huge_usage());
        let (ix, _) = latest_reported_request_usage(&msgs, &per).unwrap();
        assert_eq!(ix, 0);
    }

    #[test]
    fn forced_target_appends_unless_ends_on_compaction() {
        assert_eq!(forced_compaction_target_ix(&[]), None);
        let ends_on_compaction = vec![user("u1", "x"), compaction("c1", "s")];
        assert_eq!(forced_compaction_target_ix(&ends_on_compaction), None);
        let normal = vec![user("u1", "x"), Message::assistant(vec![])];
        assert_eq!(forced_compaction_target_ix(&normal), Some(2));
    }

    #[test]
    fn retained_keeps_text_user_messages_within_budget() {
        let msgs = vec![
            user("u1", "old prompt"),
            user("u2", "recent prompt"),
            compaction("c1", "summary"),
        ];
        let retained = retained_user_messages_before(&msgs, 2);
        assert_eq!(retained.len(), 2);
        assert_eq!(retained[0].role, Role::User);
        assert_eq!(retained[1].string_contents(), "recent prompt");
    }

    #[test]
    fn retained_skips_tool_results_and_prior_compactions() {
        use crate::language_model::{LanguageModelToolResult, MessageContent};
        let tr = MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: "tu1".to_string(),
            tool_name: "Bash".into(),
            is_error: false,
            content: "out".to_string(),
        });
        let mut with_tool = Message::user_with_content(vec![tr]);
        with_tool.id = "ut".to_string();
        let msgs = vec![
            compaction("c0", "old summary"),
            with_tool,
            user("u1", "real prompt"),
            compaction("c1", "new summary"),
        ];
        let retained = retained_user_messages_before(&msgs, 3);
        // Only "real prompt" survives; prior compaction and tool result skipped.
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].string_contents(), "real prompt");
    }

    #[test]
    fn retained_truncates_overflow_message_to_budget() {
        // A single message larger than the budget is truncated to the budget.
        let big = "x".repeat(RETAINED_USER_MESSAGES_BYTE_BUDGET + 500);
        let msgs = vec![user("u1", &big), compaction("c1", "s")];
        let retained = retained_user_messages_before(&msgs, 1);
        assert_eq!(retained.len(), 1);
        let bytes = user_message_byte_len(&retained[0]);
        assert!(bytes <= RETAINED_USER_MESSAGES_BYTE_BUDGET);
        assert!(bytes > 0);
    }

    #[test]
    fn take_byte_prefix_respects_char_boundary() {
        // "🚀" is 4 bytes; a budget landing on its trailing byte floors to the
        // last char boundary rather than splitting the emoji.
        // budget 4 → exactly "🚀".
        let (prefix, used) = take_byte_prefix("🚀x", 4);
        assert_eq!(used, 4);
        assert_eq!(prefix, "🚀");
        // budget 2 lands inside 🚀 → floors to 0, empty prefix.
        let (prefix, used) = take_byte_prefix("🚀x", 2);
        assert_eq!(used, 0);
        assert!(prefix.is_empty());
        // ASCII: no flooring needed.
        let (prefix, used) = take_byte_prefix("abcdef", 3);
        assert_eq!(used, 3);
        assert_eq!(prefix, "abc");
    }

    fn huge_usage() -> TokenUsage {
        TokenUsage {
            input_tokens: 500_000,
            ..Default::default()
        }
    }

    #[test]
    fn coalesce_merges_consecutive_same_role() {
        use crate::language_model::MessageContent;
        let msgs = vec![
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text("a".into())],
                cache: false,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text("b".into())],
                cache: true,
            },
            LanguageModelRequestMessage {
                role: Role::Assistant,
                content: vec![MessageContent::Text("c".into())],
                cache: false,
            },
        ];
        let out = coalesce_same_role(msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, Role::User);
        assert_eq!(out[0].string_contents(), "ab");
        // Trailing segment's cache flag wins.
        assert!(out[0].cache);
        assert_eq!(out[1].role, Role::Assistant);
    }

    #[test]
    fn build_compaction_request_system_then_history_then_prompt() {
        let msgs = vec![
            user("u1", "what is 1+1"),
            Message::assistant(vec![MessageContent::Text("2".into())]),
        ];
        let req = build_compaction_request(&msgs, 2);
        // system + 2 history + trailing prompt = 4 after coalesce (no adjacent
        // same-role here: system, user, assistant, user).
        assert_eq!(req.messages.len(), 4);
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[1].role, Role::User);
        assert_eq!(req.messages[2].role, Role::Assistant);
        assert_eq!(req.messages[3].role, Role::User);
        assert!(req.tools.is_empty());
    }

    #[test]
    fn build_compaction_request_coalesces_trailing_user_run() {
        // History ends on a user message; the trailing "write summary" user
        // turn coalesces into it rather than producing two adjacent user msgs.
        let msgs = vec![
            user("u1", "q"),
            Message::assistant(vec![MessageContent::Text("a".into())]),
            user("u2", "follow-up"),
        ];
        let req = build_compaction_request(&msgs, 3);
        // system + user + assistant + (user+user coalesced) = 4
        assert_eq!(req.messages.len(), 4);
        assert_eq!(req.messages[3].role, Role::User);
        assert!(req.messages[3].string_contents().contains("follow-up"));
        assert!(
            req.messages[3]
                .string_contents()
                .contains("handoff summary")
        );
    }
}
