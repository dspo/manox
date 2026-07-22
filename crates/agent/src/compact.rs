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
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::language_model::{
    AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage, MessageContent, Role, TokenUsage,
};
use crate::message::Message;
use crate::thread::model_facing_content;

// ─── compaction state capsule ──────────────────────────────────────────────

/// Schema version for the compaction envelope. Increment when the shape of
/// [`CompactionState`] changes in a way that would confuse a reader.
const CAPSULE_VERSION: u32 = 2;

/// Runtime state snapshot embedded in every compaction message. The next
/// summarizer pass reads the most recent capsule + messages since then, so
/// it never re-processes the full history — only the delta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionState {
    pub version: u32,
    /// Absolute working directory at compaction time.
    pub cwd: String,
    /// Stable id of the last canonical message covered by this compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered_message_id: Option<String>,
    /// Active worktree branch + path, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// Current git branch name at compaction time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Bounded `git status --short --branch` snapshot, or `unavailable: ...`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_status: Option<String>,
    /// Active plan snapshot (simplified: just step titles + statuses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_steps: Option<Vec<PlanStepCapsule>>,
    /// The active goal, if one is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_mode: Option<String>,
    /// Tool names currently in the registry. Helps the next instance know what
    /// capabilities are available without re-discovering them.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub active_tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub active_skills: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub background_shells: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub artifacts: Vec<String>,
}

/// A single plan step, slimmed for the capsule (no timestamps or metadata).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStepCapsule {
    pub title: String,
    pub status: String,
}

/// The on-wire format stored in [`MessageContent::Compaction`]. Legacy
/// compactions (plain text without a JSON envelope) are treated as version 0
/// with `summary` set to the raw text.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompactionEnvelope {
    #[serde(default)]
    version: u32,
    summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<CompactionState>,
}

/// Parse a [`MessageContent::Compaction`] block into its envelope form.
/// Returns `(summary_text, state)` — `state` is `None` for legacy compactions.
pub fn parse_compaction(content: &str) -> (String, Option<CompactionState>) {
    match serde_json::from_str::<CompactionEnvelope>(content) {
        Ok(env) => (env.summary, env.state),
        Err(_) => (content.to_string(), None),
    }
}

/// Latest valid deterministic state capsule in canonical history. Malformed
/// and legacy compactions are skipped, allowing thread/UI restoration to fall
/// back to the most recent usable capsule without trusting LLM prose.
pub fn latest_compaction_state(messages: &[Message]) -> Option<CompactionState> {
    messages.iter().rev().find_map(|message| {
        message.content.iter().rev().find_map(|content| {
            let MessageContent::Compaction(raw) = content else {
                return None;
            };
            parse_compaction(raw).1
        })
    })
}

/// Build the JSON envelope for a new compaction. The caller provides the
/// summary text and the current runtime state.
pub fn build_compaction_envelope(summary: String, state: CompactionState) -> String {
    let envelope = CompactionEnvelope {
        version: CAPSULE_VERSION,
        summary,
        state: Some(state),
    };
    serde_json::to_string(&envelope).unwrap_or_else(|_| envelope.summary.clone())
}

/// Collect runtime state for the compaction capsule from the thread's live
/// fields. Callers pass what they have; `None` for unavailable fields.
pub struct CompactionStateInput<'a> {
    pub cwd: &'a std::path::Path,
    pub covered_message_id: Option<&'a str>,
    pub worktree_branch: Option<&'a str>,
    pub worktree_path: Option<&'a str>,
    pub git_branch: Option<&'a str>,
    pub git_status: Option<String>,
    pub plan_steps: Option<Vec<PlanStepCapsule>>,
    pub goal: Option<&'a str>,
    pub collaboration_mode: Option<&'a str>,
    pub active_tools: Vec<String>,
    pub active_skills: Vec<String>,
    pub background_shells: Vec<String>,
    pub artifacts: Vec<String>,
}

pub fn collect_compaction_state(input: CompactionStateInput<'_>) -> CompactionState {
    CompactionState {
        version: CAPSULE_VERSION,
        cwd: input.cwd.display().to_string(),
        covered_message_id: input.covered_message_id.map(str::to_string),
        worktree_branch: input.worktree_branch.map(str::to_string),
        worktree_path: input.worktree_path.map(str::to_string),
        git_branch: input.git_branch.map(str::to_string),
        git_status: input.git_status,
        plan_steps: input.plan_steps,
        goal: input.goal.map(str::to_string),
        collaboration_mode: input.collaboration_mode.map(str::to_string),
        active_tools: input.active_tools,
        active_skills: input.active_skills,
        background_shells: input.background_shells,
        artifacts: input.artifacts,
    }
}

pub fn active_skills(messages: &[Message], bound: usize) -> Vec<String> {
    let mut successful_results = std::collections::HashSet::new();
    for message in &messages[..bound.min(messages.len())] {
        for content in &message.content {
            if let MessageContent::ToolResult(result) = content
                && !result.is_error
                && result.tool_name.as_ref() == crate::tools::SKILL
            {
                successful_results.insert(result.tool_use_id.as_str());
            }
        }
    }
    let mut skills = Vec::new();
    for message in &messages[..bound.min(messages.len())] {
        for content in &message.content {
            if let MessageContent::ToolUse(tool_use) = content
                && successful_results.contains(tool_use.id.as_str())
                && let Some(name) = tool_use
                    .input
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                && !skills.iter().any(|existing| existing == name)
            {
                skills.push(name.to_string());
            }
        }
    }
    skills
}

pub fn artifact_references(messages: &[Message], bound: usize) -> Vec<String> {
    let mut artifacts = Vec::new();
    for content in messages[..bound.min(messages.len())]
        .iter()
        .flat_map(|message| &message.content)
    {
        if let MessageContent::ToolResult(result) = content {
            for line in result.content.lines() {
                if let Some(path) = line.trim().strip_prefix("full output: ")
                    && !artifacts.iter().any(|existing| existing == path)
                {
                    artifacts.push(path.to_string());
                }
            }
        }
    }
    artifacts
}

/// Bounded, non-blocking git snapshot. Failure is data, never a compaction
/// failure; callers persist the returned `unavailable: ...` marker.
pub async fn git_status_snapshot(cwd: std::path::PathBuf) -> String {
    let (tx, rx) = async_channel::bounded(1);
    crate::runtime::handle().spawn(async move {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::process::Command::new("git")
                .args(["status", "--short", "--branch"])
                .current_dir(cwd)
                .output(),
        )
        .await;
        let text = match result {
            Ok(Ok(output)) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                let mut text = text.as_ref();
                if text.len() > 8 * 1024 {
                    text = &text[..text.floor_char_boundary(8 * 1024)];
                }
                text.trim().to_string()
            }
            Ok(Ok(output)) => format!("unavailable: git exited {}", output.status),
            Ok(Err(error)) => format!("unavailable: {error}"),
            Err(_) => "unavailable: timed out".to_string(),
        };
        let _ = tx.send(text).await;
    });
    rx.recv()
        .await
        .unwrap_or_else(|_| "unavailable: collector stopped".to_string())
}

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
/// - no compaction already sits after the latest reported usage point (a
///   post-compaction tail has not re-filled yet);
/// - the thread does not already end on a compaction (nothing new arrived);
/// - [`effective_context_tokens`] meets the threshold (`max_input_tokens *
///   threshold_pct`). Usage is *not* required: the local estimate alone can
///   cross the threshold, so a run of requests that all fail (no usage ever
///   reported) still compacts.
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
    lang: crate::language::Language,
) -> Option<usize> {
    if !auto_compact_enabled {
        return None;
    }
    if max_input_tokens < MIN_COMPACTION_CONTEXT_WINDOW {
        return None;
    }
    // Most recent user message that already has reported usage — the last
    // request's token count is part of the trigger signal. Shared with the
    // cockpit budget display so the UI and the trigger agree on "current"
    // usage.
    let usage = latest_reported_request_usage(messages, per_request);
    // If a compaction already covers the region past the usage point, the
    // post-compaction tail has not re-filled yet — nothing to do.
    if let Some((usage_ix, _)) = usage
        && let Some(c_ix) = latest_compaction_ix(messages, messages.len())
        && c_ix > usage_ix
    {
        return None;
    }
    // A thread that already ends on a compaction has nothing new to summarize.
    if messages.last().is_some_and(is_compaction) {
        return None;
    }
    let threshold = ((max_input_tokens as f64) * threshold_pct).ceil() as u64;
    if effective_context_tokens(messages, per_request, lang) < threshold {
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

/// The context fill shared by the auto-compaction trigger and the cockpit
/// budget display: the larger of the provider-reported usage and a local
/// bytes/4 estimate of the live history. The estimate keeps the trigger
/// honest when usage is missing or untrustworthy — a request rejected by the
/// provider (e.g. 400) reports no usage at all, so a purely usage-driven
/// trigger stays blind exactly when the context is oversize.
pub fn effective_context_tokens(
    messages: &[Message],
    per_request: &HashMap<String, TokenUsage>,
    lang: crate::language::Language,
) -> u64 {
    let provider = latest_reported_request_usage(messages, per_request)
        .map(|(_, u)| active_tokens(u))
        .unwrap_or(0);
    let local_estimate = (estimate_request_body_bytes(messages, lang) / 4) as u64;
    provider.max(local_estimate)
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
    lang: crate::language::Language,
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
        let request_message = to_request_message(message, lang);
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
fn to_request_message(m: &Message, lang: crate::language::Language) -> LanguageModelRequestMessage {
    LanguageModelRequestMessage {
        role: m.role,
        content: m
            .content
            .iter()
            .map(|c| model_facing_content(c, lang))
            .collect(),
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
pub fn estimate_request_body_bytes(messages: &[Message], lang: crate::language::Language) -> usize {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(|c| {
            model_facing_content(c, lang)
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

/// Build the side-LLM request that produces a compaction summary.
///
/// When a prior compaction exists before `insertion_ix`, the summarizer runs in
/// **incremental mode**: it receives the previous summary (as a user-role
/// preamble) plus only the new messages after that compaction — it never
/// re-processes the full history. Without a prior compaction, the summarizer
/// sees the full transcript before `insertion_ix` (current behavior).
///
/// Sub-agent nesting is collapsed: the summarizer is a one-shot task, not the
/// agent in agent-mode.
pub fn build_compaction_request(
    messages: &[Message],
    insertion_ix: usize,
    lang: crate::language::Language,
) -> LanguageModelRequest {
    let bound = insertion_ix.min(messages.len());
    let mut request_messages: Vec<LanguageModelRequestMessage> = Vec::new();

    // ── system prompt ──────────────────────────────────────────────────
    request_messages.push(LanguageModelRequestMessage {
        role: Role::System,
        content: vec![MessageContent::Text(
            crate::prompt::render_static(
                crate::prompt::PromptTemplate::SideCallCompactSystem,
                lang,
            )
            .expect("compact system prompt render"),
        )],
        cache: false,
    });

    // ── incremental or full history ────────────────────────────────────
    // Find the most recent compaction before the insertion point.
    let prev_compaction = latest_compaction_ix(messages, bound);

    if let Some(prev_ix) = prev_compaction {
        // Incremental mode: summarizer sees the previous summary as context
        // plus only the new messages since that compaction.
        let prev_content: String = messages[prev_ix]
            .content
            .iter()
            .filter_map(|c| {
                if let MessageContent::Compaction(text) = c {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let (prev_summary, prev_state) = parse_compaction(&prev_content);
        let state_context = prev_state
            .as_ref()
            .and_then(|state| serde_json::to_string(state).ok())
            .map(|state| format!("\nRuntime state capsule: {state}"))
            .unwrap_or_default();

        // Inject the previous summary as a user-role context block.
        request_messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text(format!(
                "Previous compaction summary (use this as context; summarize ONLY the new messages below):\n\n{prev_summary}{state_context}"
            ))],
            cache: false,
        });

        // Feed only the new messages after the previous compaction.
        for m in &messages[prev_ix + 1..bound] {
            request_messages.push(LanguageModelRequestMessage {
                role: m.role,
                content: m
                    .content
                    .iter()
                    .map(|c| model_facing_content(c, lang))
                    .collect(),
                cache: false,
            });
        }
    } else {
        // Full mode: no prior compaction — summarize the entire history.
        for m in &messages[..bound] {
            request_messages.push(LanguageModelRequestMessage {
                role: m.role,
                content: m
                    .content
                    .iter()
                    .map(|c| model_facing_content(c, lang))
                    .collect(),
                cache: false,
            });
        }
    }

    // ── final instruction ──────────────────────────────────────────────
    request_messages.push(LanguageModelRequestMessage {
        role: Role::User,
        content: vec![MessageContent::Text(
            crate::prompt::render_static(
                crate::prompt::PromptTemplate::SideCallCompactFinalInstruction,
                lang,
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
        reasoning_effort: crate::settings::side_call_effort(
            &crate::settings::side_calls().compaction_policy(),
            crate::language_model::RequestReasoningEffort::Medium,
        ),
        max_output_tokens: crate::settings::side_call_output_cap(
            crate::settings::side_calls().compaction_policy(),
        ),
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
            auto_compaction_target_ix(
                &msgs,
                &per,
                false,
                200_000,
                0.9,
                crate::language::Language::En
            ),
            None
        );
    }

    #[test]
    fn auto_target_none_below_min_window() {
        let msgs = vec![user("u1", "hi")];
        let mut per = HashMap::new();
        per.insert("u1".to_string(), huge_usage());
        assert_eq!(
            auto_compaction_target_ix(
                &msgs,
                &per,
                true,
                10_000,
                0.9,
                crate::language::Language::En
            ),
            None
        );
    }

    #[test]
    fn auto_target_none_without_usage_and_small_estimate() {
        // No reported usage and a tiny local estimate — nothing to compact.
        let msgs = vec![user("u1", "hi")];
        assert_eq!(
            auto_compaction_target_ix(
                &msgs,
                &HashMap::new(),
                true,
                200_000,
                0.9,
                crate::language::Language::En
            ),
            None
        );
    }

    #[test]
    fn auto_target_fires_on_local_estimate_without_usage() {
        // The floor: a giant history with NO reported usage (e.g. every
        // request so far failed with a 400, which reports nothing) must still
        // trigger compaction — this is the thread-2b1a37c7 regression. The
        // trailing untracked message (the flood itself) stays raw; everything
        // before it is summarized.
        let big = "x".repeat(900 * 1024); // ~225k estimated tokens > 180k threshold
        let msgs = vec![user("u1", "start"), user("u2", &big)];
        assert_eq!(
            auto_compaction_target_ix(
                &msgs,
                &HashMap::new(),
                true,
                200_000,
                0.9,
                crate::language::Language::En
            ),
            Some(1)
        );
    }

    #[test]
    fn auto_target_estimate_wins_over_smaller_provider_usage() {
        // effective = max(provider, estimate): the estimate dominates when the
        // provider under-reports (middleware-rewritten bodies, missing usage).
        let big = "x".repeat(900 * 1024);
        let msgs = vec![user("u1", "start"), user("u2", &big)];
        let mut per = HashMap::new();
        per.insert(
            "u1".to_string(),
            TokenUsage {
                input_tokens: 1_000,
                ..Default::default()
            },
        );
        assert_eq!(
            auto_compaction_target_ix(
                &msgs,
                &per,
                true,
                200_000,
                0.9,
                crate::language::Language::En
            ),
            Some(1)
        );
    }

    #[test]
    fn auto_target_none_when_thread_ends_on_compaction() {
        // Nothing new arrived after the last compaction — never re-compact.
        let msgs = vec![user("u1", "old"), compaction("c1", "sum")];
        let mut per = HashMap::new();
        per.insert("u1".to_string(), huge_usage());
        assert_eq!(
            auto_compaction_target_ix(
                &msgs,
                &per,
                true,
                200_000,
                0.9,
                crate::language::Language::En
            ),
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
            auto_compaction_target_ix(
                &msgs,
                &per,
                true,
                200_000,
                0.9,
                crate::language::Language::En
            ),
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
            auto_compaction_target_ix(
                &msgs,
                &per,
                true,
                200_000,
                0.9,
                crate::language::Language::En
            ),
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
            auto_compaction_target_ix(
                &msgs,
                &per,
                true,
                200_000,
                0.9,
                crate::language::Language::En
            ),
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
            auto_compaction_target_ix(
                &msgs,
                &per,
                true,
                window,
                0.8,
                crate::language::Language::En
            ),
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
            auto_compaction_target_ix(
                &msgs,
                &per,
                true,
                window,
                0.8,
                crate::language::Language::En
            ),
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
            auto_compaction_target_ix(
                &msgs,
                &per,
                true,
                200_000,
                0.9,
                crate::language::Language::En
            ),
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
        let retained = retained_user_messages_before(&msgs, 2, crate::language::Language::En);
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
        let retained = retained_user_messages_before(&msgs, 3, crate::language::Language::En);
        // Only "real prompt" survives; prior compaction and tool result skipped.
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].string_contents(), "real prompt");
    }

    #[test]
    fn retained_truncates_overflow_message_to_budget() {
        // A single message larger than the budget is truncated to the budget.
        let big = "x".repeat(RETAINED_USER_MESSAGES_BYTE_BUDGET + 500);
        let msgs = vec![user("u1", &big), compaction("c1", "s")];
        let retained = retained_user_messages_before(&msgs, 1, crate::language::Language::En);
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
        let req = build_compaction_request(&msgs, 2, crate::language::Language::En);
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
        let req = build_compaction_request(&msgs, 3, crate::language::Language::En);
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

    fn complete_state() -> CompactionState {
        collect_compaction_state(CompactionStateInput {
            cwd: std::path::Path::new("/repo"),
            covered_message_id: Some("covered-42"),
            worktree_branch: Some("feature/replay"),
            worktree_path: Some("/repo/.worktrees/replay"),
            git_branch: Some("feature/replay"),
            git_status: Some("## feature/replay\n M src/lib.rs".into()),
            plan_steps: Some(vec![PlanStepCapsule {
                title: "verify replay".into(),
                status: "in_progress".into(),
            }]),
            goal: Some("finish issue 299"),
            collaboration_mode: Some("default"),
            active_tools: vec!["Read".into(), "Code".into()],
            active_skills: vec!["github".into()],
            background_shells: vec!["shell-7: cargo test".into()],
            artifacts: vec!["docs/report.md".into()],
        })
    }

    #[test]
    fn compaction_envelope_round_trips_complete_deterministic_state() {
        let expected = complete_state();
        let encoded = build_compaction_envelope("handoff".into(), expected.clone());
        let (summary, state) = parse_compaction(&encoded);
        assert_eq!(summary, "handoff");
        assert_eq!(state, Some(expected));
    }

    #[test]
    fn legacy_and_malformed_compactions_fall_back_to_plain_summary() {
        for raw in ["legacy handoff", r#"{"version":2,"summary":17}"#] {
            let (summary, state) = parse_compaction(raw);
            assert_eq!(summary, raw);
            assert!(state.is_none());
        }
    }

    #[test]
    fn latest_compaction_state_skips_newer_legacy_entry() {
        let envelope = build_compaction_envelope("valid".into(), complete_state());
        let messages = vec![
            compaction("valid", &envelope),
            compaction("legacy", "plain legacy summary"),
        ];
        assert_eq!(latest_compaction_state(&messages), Some(complete_state()));
    }

    #[test]
    fn incremental_compaction_includes_previous_capsule_and_only_delta() {
        let envelope = build_compaction_envelope("previous handoff".into(), complete_state());
        let messages = vec![
            user("old", "must not be replayed"),
            compaction("capsule", &envelope),
            user("delta", "new delta only"),
        ];
        let request =
            build_compaction_request(&messages, messages.len(), crate::language::Language::En);
        let rendered = request
            .messages
            .iter()
            .map(LanguageModelRequestMessage::string_contents)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("previous handoff"));
        assert!(rendered.contains("covered-42"));
        assert!(rendered.contains("new delta only"));
        assert!(!rendered.contains("must not be replayed"));
    }
}
