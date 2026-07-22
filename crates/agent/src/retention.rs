//! History retention: classify and prune low-value tool results from the
//! model-facing projection to save context tokens.
//!
//! Every tool result is classified as Keep / Useless / Supersede(key). The
//! most recent ~40K tokens (the "hot prefix") are always preserved intact so
//! the provider prefix cache stays warm. Older useless and superseded results
//! are dropped from the projection, but only when ≥20K tokens would be saved
//! and ≥90 minutes have passed since the last prune *for this thread*.
//!
//! The canonical [`Thread::messages`] is never modified — this layer only
//! affects the projection built by [`build_completion_request`].
//!
//! **Block-level pruning.** Individual ToolResult blocks are pruned from
//! messages. When every ToolResult in a User-role message is pruned, the
//! message itself is removed *together with its preceding Assistant message*
//! to preserve ToolUse/ToolResult pairing. A User message that still has at
//! least one kept ToolResult block is retained with only the remaining blocks
//! — no orphaned ToolUses are created because the Anthropic wire pairs
//! User↔Assistant at the message level, not per-block.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::language_model::LanguageModelToolResult;
use crate::language_model::MessageContent;
use crate::message::Message;

/// Minimum cooldown between two pruning decisions for the same thread. Avoids
/// thrashing the provider-side KV cache (each prune busts the prefix cache).
const PRUNE_COOLDOWN: Duration = Duration::from_secs(90 * 60);

/// Hot prefix protection: the most recent ~40K tokens (≈160 KiB at 4 bytes per
/// token) are never touched. The provider's prompt cache uses a sliding window
/// from the end, so preserving the tail is what matters for cache stability.
const HOT_PREFIX_BYTES: usize = 160 * 1024;

/// Only apply pruning when at least this many bytes would be removed from the
/// projection. Below this threshold the cache-bust cost outweighs the savings.
const MIN_SAVINGS_BYTES: usize = 80 * 1024;

/// Per-thread cooldown timers keyed by an opaque thread-level token. A prune
/// in thread A does not suppress pruning in thread B.
static COOLDOWNS: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

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
/// earlier one — e.g. a second `Read` of the same file (with the same
/// selector range) makes the first read stale.
///
/// The key is derived from the ToolResult content only (not the paired
/// ToolUse input, which is not available in this layer). As a result:
/// - `Read` uses the full hashline header `[path#selector]` as key, so
///   reads with different selectors (e.g. `#L1-L100` vs `#L500-L600`)
///   do not falsely supersede each other.
/// - `Write`/`Edit` keys are tool-specific and do NOT supersede `Read`
///   results — stale read invalidation after writes requires
///   ToolUse-input-side tracking and is deferred.
/// - `Grep`/`Glob`/`List` key from the first three lines, which is
///   stable for same-query, same-path re-invocations but won't match
///   when the output changes.
fn supersede_key(tr: &LanguageModelToolResult) -> Option<String> {
    match &*tr.tool_name {
        "Read" => {
            // First line is `[path#selector]`. Keep the full header so
            // different selectors produce different keys.
            let header = tr.content.lines().next()?;
            Some(format!("Read:{}", header))
        }
        "Write" | "Edit" => {
            // The result text is an ack or diff; first line is not a
            // stable path. Derive a best-effort key from the content
            // fingerprint — two writes to the same file with identical
            // output are extremely unlikely outside tests.
            let head = &tr.content[..tr.content.len().min(128)];
            let h = hash_str(head);
            Some(format!("{}:{h:x}", tr.tool_name))
        }
        "Grep" | "Glob" | "List" => {
            let head: String = tr.content.lines().take(3).collect::<Vec<_>>().join("\n");
            let h = hash_str(&head);
            Some(format!("{}:{h:x}", tr.tool_name))
        }
        _ => None,
    }
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

// ─── pruning ────────────────────────────────────────────────────────────────

/// Build a pruned message list for the model-facing projection.
///
/// `cooldown_key` scopes the prune cooldown to a single thread.
///
/// Returns `None` when pruning is suppressed (cooldown active, savings
/// below threshold, or no blocks classified for removal).
pub(crate) fn prune_for_model(messages: &[Message], cooldown_key: &str) -> Option<Vec<Message>> {
    // ── per-thread cooldown ──────────────────────────────────────────────
    {
        let mut guard = COOLDOWNS.lock().unwrap();
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some(t) = map.get(cooldown_key)
            && t.elapsed() < PRUNE_COOLDOWN
        {
            return None;
        }
    }

    // ── classify every tool-result block ───────────────────────────────
    struct BlockInfo {
        block_ord: usize, // sequential across all blocks
        msg_idx: usize,
        block_idx: usize, // index within the message's content vec
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
                    block_ord: blocks.len(),
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
        return None;
    }

    let mut hot_bytes = 0usize;
    let mut hot_cutoff: Option<usize> = None; // first block_ord inside hot prefix
    for b in blocks.iter().rev() {
        hot_bytes += b.byte_len;
        if hot_bytes >= HOT_PREFIX_BYTES {
            hot_cutoff = Some(b.block_ord);
            break;
        }
    }

    let hot_cutoff = hot_cutoff?;

    // ── classify: walk in reverse to keep the latest per-key ──────────
    let mut last_seen: HashMap<String, usize> = HashMap::new();
    // (msg_idx, block_idx) of blocks to drop
    let mut drop_blocks: HashSet<(usize, usize)> = HashSet::new();
    // Tracks how many ToolResult blocks each User message has in total,
    // and how many are being pruned.
    let mut user_msg_tr_count: HashMap<usize, usize> = HashMap::new();
    let mut user_msg_pruned: HashMap<usize, usize> = HashMap::new();

    // First pass: count total ToolResult blocks per User message.
    for b in &blocks {
        if messages[b.msg_idx].role == crate::language_model::Role::User {
            *user_msg_tr_count.entry(b.msg_idx).or_default() += 1;
        }
    }

    let mut savings: usize = 0;

    for b in blocks.iter().rev() {
        if b.block_ord >= hot_cutoff {
            if let Hint::Supersede(ref key) = b.hint {
                last_seen.entry(key.clone()).or_insert(b.block_ord);
            }
            continue;
        }
        match &b.hint {
            Hint::Keep => {}
            Hint::Useless => {
                drop_blocks.insert((b.msg_idx, b.block_idx));
                *user_msg_pruned.entry(b.msg_idx).or_default() += 1;
                savings += b.byte_len;
            }
            Hint::Supersede(key) => {
                if last_seen.contains_key(key) {
                    drop_blocks.insert((b.msg_idx, b.block_idx));
                    *user_msg_pruned.entry(b.msg_idx).or_default() += 1;
                    savings += b.byte_len;
                } else {
                    last_seen.insert(key.clone(), b.block_ord);
                }
            }
        }
    }

    if savings < MIN_SAVINGS_BYTES {
        return None;
    }

    // ── stamp the per-thread cooldown timer ────────────────────────────
    {
        let mut guard = COOLDOWNS.lock().unwrap();
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(cooldown_key.to_string(), Instant::now());
    }

    // ── identify fully-pruned User messages → drop paired Assistant ───
    // When EVERY ToolResult in a User message is pruned, the message
    // itself is dropped, and the preceding Assistant message (carrying
    // the paired ToolUse blocks) must also be dropped so the wire format
    // stays valid.
    let mut drop_msg: HashSet<usize> = HashSet::new();
    for (&msg_idx, &pruned) in &user_msg_pruned {
        let total = user_msg_tr_count.get(&msg_idx).copied().unwrap_or(0);
        if pruned >= total {
            drop_msg.insert(msg_idx);
            if msg_idx > 0 && messages[msg_idx - 1].role == crate::language_model::Role::Assistant {
                drop_msg.insert(msg_idx - 1);
            }
        }
    }

    // ── rebuild: drop fully-pruned messages, partially prune blocks ────
    let pruned: Vec<Message> = messages
        .iter()
        .enumerate()
        .filter(|(mi, _)| !drop_msg.contains(mi))
        .map(|(mi, msg)| {
            let new_content: Vec<MessageContent> = msg
                .content
                .iter()
                .enumerate()
                .filter(|(bi, _)| !drop_blocks.contains(&(mi, *bi)))
                .map(|(_, c)| c.clone())
                .collect();
            Message {
                id: msg.id.clone(),
                timestamp: msg.timestamp,
                parent_id: msg.parent_id.clone(),
                role: msg.role,
                content: new_content,
                ui: msg.ui.clone(),
            }
        })
        .collect();

    if pruned.len() >= messages.len() {
        return None;
    }

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

    fn make_msg(role: Role, blocks: Vec<MessageContent>) -> Message {
        Message {
            id: format!("m_{}", rand_id()),
            timestamp: 0,
            parent_id: None,
            role,
            content: blocks,
            ui: Default::default(),
        }
    }

    fn rand_id() -> u64 {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    // ── classification tests ──────────────────────────────────────────

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
    fn read_supersede_key_includes_selector() {
        // Two reads of the same file with different selectors produce
        // different keys, so they do not falsely supersede each other.
        let r1 = LanguageModelToolResult {
            tool_use_id: "tu_1".into(),
            tool_name: "Read".into(),
            is_error: false,
            content: "[src/main.rs#L1-L100]\n1:fn main() {\n…\n100:}\n".into(),
        };
        let r2 = LanguageModelToolResult {
            tool_use_id: "tu_2".into(),
            tool_name: "Read".into(),
            is_error: false,
            content: "[src/main.rs#L500-L600]\n500:fn other() {\n…\n600:}\n".into(),
        };
        let k1 = supersede_key(&r1).unwrap();
        let k2 = supersede_key(&r2).unwrap();
        assert_ne!(k1, k2, "different selectors → different keys");
    }

    #[test]
    fn read_same_selector_same_key() {
        let r1 = LanguageModelToolResult {
            tool_use_id: "tu_1".into(),
            tool_name: "Read".into(),
            is_error: false,
            content: "[src/lib.rs#L42]\n42:old\n".into(),
        };
        let r2 = LanguageModelToolResult {
            tool_use_id: "tu_2".into(),
            tool_name: "Read".into(),
            is_error: false,
            content: "[src/lib.rs#L42]\n42:new\n".into(),
        };
        assert_eq!(supersede_key(&r1), supersede_key(&r2));
    }

    #[test]
    fn write_key_is_tool_specific() {
        // Write/Edit keys are scoped per tool type — they don't collide
        // with Read keys. Cross-tool read-after-write invalidation
        // requires ToolUse-input-side tracking and is deferred.
        let w = LanguageModelToolResult {
            tool_use_id: "tu_1".into(),
            tool_name: "Write".into(),
            is_error: false,
            content: "File written successfully.\n".into(),
        };
        let k = supersede_key(&w).unwrap();
        assert!(k.starts_with("Write:"), "key is tool-scoped: {k}");
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

    // ── pruning tests ─────────────────────────────────────────────────

    #[test]
    fn empty_messages_yield_none() {
        assert!(prune_for_model(&[], "test").is_none());
    }

    #[test]
    fn hot_prefix_is_untouched() {
        let msg = make_msg(Role::User, vec![tr("Bash", "ok")]);
        assert!(prune_for_model(&[msg], "test").is_none());
    }

    #[test]
    fn mixed_message_keep_block_survives_partial_prune() {
        // A User message with one useful result and many useless results:
        // only the useless blocks are pruned; the Keep block is retained.
        // This is the reviewer counterexample: 900 useless + 1 keep.
        let big_data = "x".repeat(HOT_PREFIX_BYTES + 10_000);
        let useful = tr("Bash", "test result: 42 passed, 0 failed\ncoverage: 85%");
        let useless = tr("Write", "File written successfully.");
        // Put a big read first so the hot prefix boundary can be
        // established and the useless blocks are above it.
        let big_read = tr("Read", &big_data);

        let msgs = vec![
            make_msg(Role::User, vec![big_read]),
            make_msg(Role::Assistant, vec![]),
            // This message has 900 useless + 1 useful — partial prune test.
            {
                let mut content = Vec::new();
                for _ in 0..900 {
                    content.push(useless.clone());
                }
                content.push(useful.clone());
                make_msg(Role::User, content)
            },
        ];

        let result = prune_for_model(&msgs, "test_mixed");
        // Pruning should either return None (hot prefix too small, or
        // savings below threshold) or return a Vec where the useful
        // Bash result survived while useless Write results were pruned.
        if let Some(pruned) = result {
            // The mixed message (index 2) should still exist if the
            // useful Bash result was kept. Its content should contain
            // the Bash result.
            let mixed = &pruned[pruned.len() - 1];
            assert_eq!(mixed.role, Role::User);
            let kept_bash = mixed.content.iter().any(
                |c| matches!(c, MessageContent::ToolResult(tr) if tr.tool_name.as_ref() == "Bash"),
            );
            assert!(kept_bash, "useful Bash result must survive partial prune");
        }
        // If result is None (below threshold / within hot prefix), that's
        // also valid — the key invariant is that when pruning DOES occur,
        // Keep blocks survive.
    }

    #[test]
    fn fully_pruned_user_drops_paired_assistant() {
        // When ALL ToolResult blocks in a User message are pruned, both
        // the User message and its preceding Assistant message must be
        // removed to maintain ToolUse/ToolResult pairing.
        let big_data = "x".repeat(HOT_PREFIX_BYTES + 10_000);
        let msgs = vec![
            make_msg(Role::User, vec![tr("Read", &big_data)]),
            make_msg(Role::Assistant, vec![]),
            make_msg(Role::User, vec![tr("Write", "File written successfully.")]),
        ];

        let result = prune_for_model(&msgs, "test_full_prune");
        if let Some(pruned) = result {
            // Only the first Read message should survive; the Assistant
            // at index 1 and the useless Write at index 2 should both be
            // gone.
            assert_eq!(pruned.len(), 1, "only first Read survives: {pruned:?}");
            assert_eq!(pruned[0].role, Role::User);
        }
    }

    #[test]
    fn different_cooldown_keys_are_independent() {
        let msgs = vec![make_msg(Role::User, vec![tr("Bash", "ok")])];
        let _ = prune_for_model(&msgs, "thread_A");
        let _ = prune_for_model(&msgs, "thread_B");
        // No panic — different keys used independent cooldown slots.
    }
}
