//! History retention: classify and prune low-value tool results from the
//! model-facing projection to save context tokens.
//!
//! Every tool result is classified as Keep / Useless / Supersede(key). The
//! most recent ~40K tokens (the "hot prefix") are always preserved intact so
//! the provider prefix cache stays warm. Older useless and superseded results
//! are dropped from the projection, but only when ≥20K tokens would be saved
//! and ≥90 minutes have passed since the last prune.
//!
//! The canonical [`Thread::messages`] is never modified — this layer only
//! affects the projection built by [`build_completion_request`].

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::language_model::{LanguageModelToolResult, MessageContent};
use crate::message::Message;

/// Minimum cooldown between two pruning decisions. Avoids thrashing the prefix
/// cache (each prune busts the provider-side KV cache).
const PRUNE_COOLDOWN: Duration = Duration::from_secs(90 * 60);

/// Hot prefix protection: the most recent ~40K tokens (≈160 KiB at 4 bytes per
/// token) are never touched. The provider's prompt cache uses a sliding window
/// from the end, so preserving the tail is what matters for cache stability.
const HOT_PREFIX_BYTES: usize = 160 * 1024;

/// Only apply pruning when at least this many bytes would be removed from the
/// projection. Below this threshold the cache-bust cost outweighs the savings.
const MIN_SAVINGS_BYTES: usize = 80 * 1024;

static LAST_PRUNE: Mutex<Option<Instant>> = Mutex::new(None);

// ─── classification ────────────────────────────────────────────────────────

/// Retention classification for a single tool-result block.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Hint {
    /// Keep — carries lasting information.
    Keep,
    /// Useless — pure acknowledgment, no information (e.g. "File written.").
    Useless,
    /// Superseded — a newer result with `key` replaces this one.
    Supersede(String),
}

/// Classify a tool result.
fn classify(tr: &LanguageModelToolResult) -> Hint {
    if is_useless(tr) {
        return Hint::Useless;
    }
    if let Some(key) = supersede_key(tr) {
        return Hint::Supersede(key);
    }
    Hint::Keep
}

/// A tool result is useless when it is a short, data-free confirmation.
fn is_useless(tr: &LanguageModelToolResult) -> bool {
    if tr.is_error {
        return false;
    }
    let t = tr.content.trim();
    if t.is_empty() {
        return true;
    }
    // Short ack-only results: "File written successfully." / "Done." / "OK."
    if t.len() <= 120 && t.lines().count() <= 2 {
        let lower = t.to_lowercase();
        let has_ack = lower.contains("success")
            || lower.contains("written")
            || lower.contains("created")
            || lower.contains("deleted")
            || lower.contains("ok")
            || lower.contains("done");
        // A data-rich result has structured content beyond the ack: colons,
        // counts, or multi-line detail. Plain "File written successfully."
        // is pure ack — use it only to confirm operation success, not as
        // context that carries lasting state.
        let has_data = lower.contains(": ")
            || lower.contains("line")
            || lower.contains("error")
            || lower.contains("result")
            || t.lines().any(|l| l.len() > 80);
        if has_ack && !has_data {
            return true;
        }
    }
    false
}

/// Compute a supersede key. A later result with the same key replaces an
/// earlier one — e.g. a second `Read` of the same file makes the first read
/// stale.
fn supersede_key(tr: &LanguageModelToolResult) -> Option<String> {
    match &*tr.tool_name {
        "Read" | "Write" | "Edit" => {
            // First line is the path header from read_file or the path arg.
            tr.content
                .lines()
                .next()
                .map(|l| format!("{}:{}", tr.tool_name, l))
        }
        "Grep" | "Glob" | "List" => {
            // The query / path pattern is in the first couple of lines.
            let head: String = tr.content.lines().take(3).collect::<Vec<_>>().join("\n");
            Some(format!("{}:{}", tr.tool_name, head))
        }
        _ => None,
    }
}

// ─── pruning ────────────────────────────────────────────────────────────────

/// Build a pruned message list for the model-facing projection. Returns
/// `None` when pruning is suppressed (cooldown active, or savings below the
/// minimum threshold).
pub(crate) fn prune_for_model(messages: &[Message]) -> Option<Vec<Message>> {
    // ── cooldown ────────────────────────────────────────────────────────
    {
        let last = LAST_PRUNE.lock().unwrap();
        if let Some(t) = *last
            && t.elapsed() < PRUNE_COOLDOWN
        {
            return None;
        }
    }

    // ── classify every tool-result block ───────────────────────────────
    struct BlockInfo {
        msg_idx: usize,
        block_idx: usize,
        hint: Hint,
        byte_len: usize,
    }
    let mut blocks: Vec<BlockInfo> = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        for (bi, block) in msg.content.iter().enumerate() {
            if let MessageContent::ToolResult(tr) = block {
                let hint = classify(tr);
                let byte_len = tr.content.len();
                blocks.push(BlockInfo {
                    msg_idx: mi,
                    block_idx: bi,
                    hint,
                    byte_len,
                });
            }
        }
    }

    // ── walk backward to find the hot prefix boundary ─────────────────
    let total_bytes: usize = blocks.iter().map(|b| b.byte_len).sum();
    if total_bytes <= HOT_PREFIX_BYTES {
        return None; // Everything is hot prefix — nothing to prune.
    }

    let mut hot_bytes = 0usize;
    let mut hot_cutoff: Option<usize> = None; // first block index inside hot prefix
    for (i, b) in blocks.iter().enumerate().rev() {
        hot_bytes += b.byte_len;
        if hot_bytes >= HOT_PREFIX_BYTES {
            hot_cutoff = Some(i);
            break;
        }
    }

    let hot_cutoff = hot_cutoff?;

    // ── apply pruning above the hot prefix ────────────────────────────
    // For Supersede: per-key, keep only the last occurrence (highest index).
    // For Useless: drop.
    let mut last_seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut drop_set: Vec<(usize, usize)> = Vec::new(); // (msg_idx, block_idx)
    let mut savings: usize = 0;

    for b in &blocks {
        if b.msg_idx >= hot_cutoff {
            // Inside hot prefix — never drop, but still track supersede keys
            // so later (hot) results can supersede earlier (cold) ones.
            if let Hint::Supersede(ref key) = b.hint {
                last_seen.insert(key.clone(), b.msg_idx);
            }
            continue;
        }
        match &b.hint {
            Hint::Keep => {}
            Hint::Useless => {
                drop_set.push((b.msg_idx, b.block_idx));
                savings += b.byte_len;
            }
            Hint::Supersede(key) => {
                if last_seen.contains_key(key) {
                    // A newer result with this key exists; drop this one.
                    drop_set.push((b.msg_idx, b.block_idx));
                    savings += b.byte_len;
                } else {
                    last_seen.insert(key.clone(), b.msg_idx);
                }
            }
        }
    }

    if savings < MIN_SAVINGS_BYTES {
        return None;
    }

    // ── stamp the cooldown timer ──────────────────────────────────────
    {
        let mut last = LAST_PRUNE.lock().unwrap();
        *last = Some(Instant::now());
    }

    // ── rebuild the message list with pruned blocks ───────────────────
    let pruned: Vec<Message> = messages
        .iter()
        .enumerate()
        .filter_map(|(mi, msg)| {
            let new_content: Vec<MessageContent> = msg
                .content
                .iter()
                .enumerate()
                .filter(|(bi, _)| !drop_set.contains(&(mi, *bi)))
                .map(|(_, c)| c.clone())
                .collect();
            if new_content.is_empty() {
                // Drop entire message if all its blocks were pruned.
                None
            } else {
                Some(Message {
                    id: msg.id.clone(),
                    timestamp: msg.timestamp,
                    parent_id: msg.parent_id.clone(),
                    role: msg.role,
                    content: new_content,
                    ui: msg.ui.clone(),
                })
            }
        })
        .collect();

    Some(pruned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::Role;

    fn tr(name: &str, content: &str) -> MessageContent {
        MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: "tu_1".into(),
            tool_name: name.into(),
            is_error: false,
            content: content.into(),
        })
    }

    #[test]
    fn empty_result_is_useless() {
        let r = LanguageModelToolResult {
            tool_use_id: "tu_1".into(),
            tool_name: "Bash".into(),
            is_error: false,
            content: String::new(),
        };
        assert!(is_useless(&r));
    }

    #[test]
    fn ack_only_is_useless() {
        let r = LanguageModelToolResult {
            tool_use_id: "tu_1".into(),
            tool_name: "Write".into(),
            is_error: false,
            content: "File written successfully.".into(),
        };
        assert!(is_useless(&r));
    }

    #[test]
    fn error_is_not_useless() {
        let r = LanguageModelToolResult {
            tool_use_id: "tu_1".into(),
            tool_name: "Bash".into(),
            is_error: true,
            content: "command not found: foo".into(),
        };
        assert!(!is_useless(&r));
    }

    #[test]
    fn data_rich_result_is_not_useless() {
        let r = LanguageModelToolResult {
            tool_use_id: "tu_1".into(),
            tool_name: "Bash".into(),
            is_error: false,
            content: "test result: 42 tests passed, 0 failed\ncover: 85%".into(),
        };
        assert!(!is_useless(&r));
    }

    #[test]
    fn read_file_gets_supersede_key() {
        let r = LanguageModelToolResult {
            tool_use_id: "tu_1".into(),
            tool_name: "Read".into(),
            is_error: false,
            content: "[src/main.rs#L1]\n1:fn main() {\n2:    println!(\"hi\");\n3:}\n".into(),
        };
        let key = supersede_key(&r);
        assert!(key.is_some());
        assert!(key.unwrap().contains("main.rs"));
    }

    #[test]
    fn bash_does_not_supersede() {
        let r = LanguageModelToolResult {
            tool_use_id: "tu_1".into(),
            tool_name: "Bash".into(),
            is_error: false,
            content: "cargo build succeeded".into(),
        };
        assert!(supersede_key(&r).is_none());
    }

    #[test]
    fn empty_messages_yield_none() {
        assert!(prune_for_model(&[]).is_none());
    }

    #[test]
    fn hot_prefix_is_untouched() {
        // A single short message is entirely within the hot prefix.
        let msg = Message {
            id: "m1".into(),
            timestamp: 0,
            parent_id: None,
            role: Role::Assistant,
            content: vec![tr("Bash", "ok")],
            ui: Default::default(),
        };
        // The result is useless but inside the hot prefix — pruning should skip.
        assert!(prune_for_model(&[msg]).is_none());
    }
}
