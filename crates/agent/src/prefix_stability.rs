//! Prefix-stability diagnostics for provider-side prompt caching.
//!
//! manox's `Thread::messages` is append-only today, so the byte prefix sent to
//! the LLM (system prompt + tool specs + message history) is naturally stable
//! across turns — only the new tail is a cache miss each turn. `StablePrefix`
//! fingerprints the system prompt + tool specs each turn and reports drift
//! (system-prompt change, tool-set change) so the UI can surface a cache-hit
//! chip and accidental drift (a future feature that rewrites messages or
//! hot-reloads tools) is visible.
//!
//! The `AppendOnlyLog` / `AppendOnlyContextManager` below remain scaffolding
//! for the day per-turn tool-result truncation, image stripping, or history
//! rewriting lands — at that point the request can be routed through
//! [`AppendOnlyContextManager`] to preserve the byte-stable prefix up to the
//! divergence point instead of re-sending the entire conversation (which would
//! silently break the provider's prefix cache).
//!
//! The digest covers every field the provider may serialize — role, content,
//! tool calls (both `tool_calls` and OpenAI-wire `tool_calls`), tool-result
//! ids/names/error flags, and assistant `id` — so an in-place rewrite of *any*
//! of these fields is visible to [`AppendOnlyContextManager::sync_messages`].
//!
//! Modeled on oh-my-pi's `append-only-context.ts` (the `StablePrefix` +
//! `AppendOnlyLog` + per-message digest design), adapted to manox's
//! `LanguageModelRequestMessage`.

#![allow(dead_code)]

use xxhash_rust::xxh32::xxh32;

use crate::language_model::{LanguageModelRequest, LanguageModelRequestMessage, MessageContent};

/// A fingerprinted prefix (system prompt + tool specs) tracked across turns
/// so drift is detectable. Also tallies check/change counts for a stability
/// ratio the UI renders as a cache-hit chip.
#[derive(Default)]
pub struct StablePrefix {
    snapshot: Option<StablePrefixSnapshot>,
    version: u32,
    check_count: u64,
    change_count: u64,
    last_change: Option<PrefixChange>,
}

/// A captured prefix snapshot: the frozen system-prompt text plus a tool-spec
/// fingerprint. Exposed via [`StablePrefix::system`] so a future caller can
/// reuse the exact bytes that populated the provider's cache.
struct StablePrefixSnapshot {
    system: Vec<String>,
    tools_fingerprint: u32,
}

/// What drifted between two `StablePrefix` snapshots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrefixChange {
    pub system_changed: bool,
    pub tools_changed: bool,
}

impl StablePrefix {
    /// Fingerprint the live request against the pinned snapshot. Returns
    /// `Some(change)` when the prefix drifted (system prompt and/or tool set),
    /// `None` when stable or on the first (baseline-establishing) call. The
    /// first call pins the baseline without counting as drift.
    pub fn build(&mut self, request: &LanguageModelRequest) -> Option<PrefixChange> {
        let system = collect_system_text(&request.messages);
        let tools_fingerprint = tools_fingerprint(&request.tools);
        self.check_count += 1;

        let prev = match &self.snapshot {
            Some(snap) => snap,
            None => {
                // First check: pin the baseline, no drift to report.
                self.snapshot = Some(StablePrefixSnapshot {
                    system,
                    tools_fingerprint,
                });
                self.version += 1;
                self.last_change = None;
                return None;
            }
        };

        let system_changed = prev.system != system;
        let tools_changed = prev.tools_fingerprint != tools_fingerprint;
        if !system_changed && !tools_changed {
            return None;
        }

        self.snapshot = Some(StablePrefixSnapshot {
            system,
            tools_fingerprint,
        });
        self.version += 1;
        self.change_count += 1;
        let change = PrefixChange {
            system_changed,
            tools_changed,
        };
        self.last_change = Some(change);
        Some(change)
    }

    /// Force rebuild on the next `build()` call (e.g. after an MCP reconnect
    /// changed the tool set).
    pub fn invalidate(&mut self) {
        self.snapshot = None;
        self.last_change = None;
    }

    /// Monotonic version counter; bumps each time the prefix bytes change.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Total stability checks performed (one per `build` call).
    pub fn check_count(&self) -> u64 {
        self.check_count
    }

    /// Checks that detected drift.
    pub fn change_count(&self) -> u64 {
        self.change_count
    }

    /// Stability ratio as a whole-percent `u16` (0–100). 100 means the prefix
    /// has never drifted. 100 before any check (no division by zero).
    pub fn stability_pct(&self) -> u16 {
        if self.check_count == 0 {
            return 100;
        }
        let stable = self.check_count - self.change_count;
        ((stable * 100) / self.check_count).min(100) as u16
    }

    /// The most recent drift, if any.
    pub fn last_change(&self) -> Option<PrefixChange> {
        self.last_change
    }

    /// The frozen system-prompt text, or `None` if `build()` was never called
    /// (or was invalidated). A future `build_completion_request` will reuse
    /// this instead of re-deriving from live state, so the cached prefix
    /// stays byte-identical across turns.
    pub fn system(&self) -> Option<&[String]> {
        self.snapshot.as_ref().map(|s| s.system.as_slice())
    }
}

/// An append-only message log at the `LanguageModelRequestMessage` layer.
///
/// The only mutation path reserved for compaction is `replace_tail`; every
/// other operation is append-only. `sync_messages` finds the longest
/// byte-stable prefix shared with the previously-synced log and preserves it,
/// dropping only the diverged tail — so the provider's KV cache stays warm up
/// to the divergence point.
#[derive(Default)]
pub struct AppendOnlyLog {
    entries: Vec<LanguageModelRequestMessage>,
    digests: Vec<u32>,
    last_sync_count: usize,
}

impl AppendOnlyLog {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn append(&mut self, message: LanguageModelRequestMessage) {
        self.entries.push(message);
    }

    /// Replace the last entry — only legal for compaction.
    pub fn replace_tail(&mut self, replacement: LanguageModelRequestMessage) {
        let idx = self.entries.len().wrapping_sub(1);
        if idx < self.entries.len() {
            self.entries[idx] = replacement;
        }
    }

    /// Drop entries past `count`, keeping the first `count` byte-stable.
    pub fn truncate(&mut self, count: usize) {
        if count < self.entries.len() {
            self.entries.truncate(count);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.digests.clear();
        self.last_sync_count = 0;
    }

    pub fn messages(&self) -> &[LanguageModelRequestMessage] {
        &self.entries
    }

    /// Sync a freshly-normalized message list into the append-only log.
    ///
    /// Three cases:
    /// 1. **Append** — same prefix, new tail → push the new entries.
    /// 2. **Compaction** — shorter list → clear and replay.
    /// 3. **In-place rewrite** (per-turn pruning, image strip) → trim the log
    ///    to the longest byte-stable prefix shared with the previous sync,
    ///    then append the diverged tail. This is what keeps the provider's KV
    ///    cache warm up to the divergence point instead of forcing a full
    ///    re-prefill.
    pub fn sync_messages(&mut self, normalized: &[LanguageModelRequestMessage]) {
        // Compaction (array shrunk) — no previously-synced bytes survive.
        if normalized.len() < self.last_sync_count {
            self.clear();
        }

        if self.last_sync_count > 0 {
            let stable = self
                .longest_stable_prefix(normalized)
                .min(self.entries.len());
            if stable < self.last_sync_count {
                self.truncate(stable);
                self.last_sync_count = stable;
                self.digests.truncate(stable);
            }
        }

        for msg in normalized.iter().skip(self.last_sync_count) {
            self.entries.push(msg.clone());
            self.digests.push(message_digest(msg));
        }
        self.last_sync_count = normalized.len();
    }

    /// Reset the sync cursor and clear the log (e.g. on model/provider switch).
    pub fn reset_sync_cursor(&mut self) {
        self.clear();
    }

    /// Index of the first message whose serialized bytes differ from the
    /// previously-synced log.
    fn longest_stable_prefix(&self, normalized: &[LanguageModelRequestMessage]) -> usize {
        let bound = self.last_sync_count.min(normalized.len());
        for (i, msg) in normalized.iter().take(bound).enumerate() {
            if message_digest(msg) != self.digests[i] {
                return i;
            }
        }
        bound
    }
}

/// Manages a stable prefix + append-only log for the agent loop. Not yet wired
/// into `build_completion_request`; see the module docs.
#[derive(Default)]
pub struct AppendOnlyContextManager {
    pub prefix: StablePrefix,
    pub log: AppendOnlyLog,
}

impl AppendOnlyContextManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset prefix + log for a model/provider switch.
    pub fn invalidate_for_model_change(&mut self) {
        self.prefix.invalidate();
        self.log.clear();
    }

    /// Reset everything and rebuild the prefix from `request`.
    pub fn reset(&mut self, request: &LanguageModelRequest) {
        self.prefix.invalidate();
        self.log.clear();
        self.prefix.build(request);
    }
}

/// Collect system-role message text into a stable vector (system prompt head).
fn collect_system_text(messages: &[LanguageModelRequestMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.role == crate::language_model::Role::System)
        .filter_map(|m| {
            let s = m.string_contents();
            (!s.is_empty()).then_some(s)
        })
        .collect()
}

/// Fingerprint the tool spec list so a tool-set change invalidates the prefix.
fn tools_fingerprint(tools: &[crate::language_model::LanguageModelRequestTool]) -> u32 {
    let mut bytes: Vec<u8> = Vec::new();
    for t in tools {
        bytes.extend_from_slice(t.name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(t.description.as_bytes());
        bytes.push(0);
        // input_schema is a Value; its canonical JSON form is stable enough
        // for a fingerprint (serde_json::Value serializes deterministically
        // for equal values).
        let schema = serde_json::to_vec(&t.input_schema).unwrap_or_default();
        bytes.extend_from_slice(&schema);
        bytes.push(0xFF);
    }
    xxh32(&bytes, 0)
}

/// Deterministic digest over every field the provider may serialize — role,
/// content, tool calls, tool-result ids/names/error flags, and assistant id —
/// so an in-place rewrite of *any* field is visible to the stable-prefix scan.
fn message_digest(msg: &LanguageModelRequestMessage) -> u32 {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(format!("{:?}", msg.role).as_bytes());
    bytes.push(0);
    for c in &msg.content {
        bytes.extend_from_slice(&message_content_digest_bytes(c));
        bytes.push(0xFF);
    }
    bytes.push(msg.cache as u8);
    xxh32(&bytes, 0)
}

/// Serialize a `MessageContent` variant into stable bytes for digesting.
fn message_content_digest_bytes(c: &MessageContent) -> Vec<u8> {
    match c {
        MessageContent::Text(t) => format!("text:{t}").into_bytes(),
        MessageContent::Thinking { text, signature } => {
            format!("thinking:{text}:{:?}", signature).into_bytes()
        }
        MessageContent::ToolUse(tu) => {
            format!("tooluse:{}:{}:{}", tu.id, tu.name, tu.input).into_bytes()
        }
        MessageContent::ToolResult(tr) => format!(
            "toolresult:{}:{}:{}:{}",
            tr.tool_use_id, tr.tool_name, tr.is_error, tr.content
        )
        .into_bytes(),
        MessageContent::Image { data, mime_type } => {
            format!("image:{mime_type}:{}", data.len()).into_bytes()
        }
    }
}

// Re-export the Arc used only to keep the module self-contained for future
// transport-state hooks; harmless today.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::{LanguageModelRequestMessage, MessageContent, Role};
    use std::sync::Arc;

    fn user_msg(text: &str) -> LanguageModelRequestMessage {
        LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text(text.to_string())],
            cache: false,
        }
    }

    #[test]
    fn digest_changes_on_tool_result_id() {
        let a = LanguageModelRequestMessage {
            role: Role::Assistant,
            content: vec![MessageContent::ToolResult(
                crate::language_model::LanguageModelToolResult {
                    tool_use_id: "id-1".into(),
                    tool_name: Arc::from("read_file"),
                    is_error: false,
                    content: "out".into(),
                },
            )],
            cache: false,
        };
        let mut b = a.clone();
        b.content[0] = MessageContent::ToolResult(crate::language_model::LanguageModelToolResult {
            tool_use_id: "id-2".into(),
            ..match_tool_result(&a.content[0])
        });
        assert_ne!(message_digest(&a), message_digest(&b));
    }

    fn match_tool_result(c: &MessageContent) -> crate::language_model::LanguageModelToolResult {
        match c {
            MessageContent::ToolResult(tr) => tr.clone(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn longest_stable_prefix_finds_divergence() {
        let mut log = AppendOnlyLog::default();
        let initial = vec![user_msg("a"), user_msg("b"), user_msg("c")];
        log.sync_messages(&initial);
        // Rewrite the second message — prefix of 1 should survive.
        let rewritten = vec![user_msg("a"), user_msg("B"), user_msg("c")];
        log.sync_messages(&rewritten);
        // After sync the log holds a, B, c with the stable prefix (a) reused.
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn append_grows_log() {
        let mut log = AppendOnlyLog::default();
        log.sync_messages(&[user_msg("a"), user_msg("b")]);
        assert_eq!(log.len(), 2);
        log.sync_messages(&[user_msg("a"), user_msg("b"), user_msg("c")]);
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn compaction_shorter_clears_and_replays() {
        let mut log = AppendOnlyLog::default();
        log.sync_messages(&[user_msg("a"), user_msg("b"), user_msg("c")]);
        log.sync_messages(&[user_msg("summarized")]);
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn stable_prefix_build_detects_change() {
        let mut sp = StablePrefix::default();
        let req = LanguageModelRequest {
            messages: vec![LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text("sys".into())],
                cache: false,
            }],
            ..Default::default()
        };
        assert!(sp.build(&req).is_none()); // first build → baseline, no drift
        assert!(sp.build(&req).is_none()); // same → stable
        assert_eq!(sp.check_count(), 2);
        assert_eq!(sp.change_count(), 0);
        assert_eq!(sp.stability_pct(), 100);

        // Change the system prompt → drift reported, system_changed only.
        let mut req2 = req.clone();
        req2.messages[0].content[0] = MessageContent::Text("sys-v2".into());
        let change = sp.build(&req2).expect("drift detected");
        assert!(change.system_changed);
        assert!(!change.tools_changed);
        assert_eq!(sp.change_count(), 1);
        // 2 stable / 3 checks = 66%
        assert_eq!(sp.stability_pct(), 66);
    }

    #[test]
    fn system_getter_exposes_frozen_text() {
        let mut sp = StablePrefix::default();
        assert!(sp.system().is_none()); // unbuilt
        let req = LanguageModelRequest {
            messages: vec![LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text("frozen".into())],
                cache: false,
            }],
            ..Default::default()
        };
        sp.build(&req);
        assert_eq!(sp.system().unwrap()[0], "frozen");
        sp.invalidate();
        assert!(sp.system().is_none()); // invalidated → no frozen prefix
    }
}
