//! The prefix-cache stability gate — read-only observability over the
//! provider request payloads of one thread run.
//!
//! Prompt caching rewards a byte-stable request prefix: every turn must
//! replay the previous bytes and only append. Nothing in the runtime
//! verifies that today, so a drift (a volatile line re-rendered mid-run, a
//! tool definition rewritten in place) silently re-bills the whole prompt
//! and is invisible in the metrics journal even though the
//! [`ThreadEvent::PrefixStability`] / [`ThreadEvent::CacheInvalidation`]
//! entries exist for it.
//!
//! This module closes that gap as pure observation:
//!
//! * it is attached to the stream path the production engine already uses
//!   (the harness's `BeforeProviderPayload` hook, which the kernel's
//!   bridged [`RequestObserver`] fires for every wire attempt), and
//! * it never mutates a payload — [`RequestObserver::before_payload`]
//!   always answers `None`, and the hook handler returns its context
//!   unchanged, so the model-visible bytes are exactly what they were.
//!
//! The gate keeps one snapshot per thread run (one observer instance is
//! constructed per session by [`attach_prefix_gate`]) and compares each new
//! payload against it structurally — the system block, the tool catalog, and
//! the message array item by item — because that is the granularity a
//! provider prompt cache replays. A byte prefix over the whole serialized
//! payload cannot express it: object keys are ordered by the encoder, so an
//! appended message shifts every byte after the array and a pure append would
//! read as a divergence. Divergences the runtime *intends* — compaction
//! rewriting history, a model switch, a permission-mode change, the LSP-ready
//! line appearing, the `today` date rolling over, a tool becoming visible or
//! invisible — are attributed to the [`Whitelist`] and reported without a
//! cache-miss estimate; only an unattributed divergence also emits
//! [`ThreadEvent::CacheInvalidation`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use manox_harness::core::provider::RequestObserver;
use tokio::sync::mpsc;

use crate::thread::ThreadEvent;
use crate::thread_engine::BackendNotice;

/// Bytes per token in the re-processing estimate. The gate never sees the
/// provider's tokenizer, so it prices a divergence with the conventional
/// 4-bytes-per-token heuristic; the number is an order-of-magnitude
/// telemetry figure, not a billing statement.
const APPROX_BYTES_PER_TOKEN: usize = 4;

/// A divergence the runtime produces on purpose, so it must not be
/// counted as a prompt-cache regression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whitelist {
    /// A compaction boundary rewrote the history (the message array got
    /// shorter), so the old prefix no longer exists by design.
    Compaction,
    /// The model changed: the cache key changes with it.
    ModelSwitch,
    /// The permission mode changed, re-rendering a mode line.
    PermissionMode,
    /// The LSP-ready line entered or left the system prompt.
    LspReadyLine,
    /// The `today` date rolled over inside the system prompt.
    TodayRollover,
    /// A tool became visible or invisible: the catalog changed shape, not
    /// content — every retained definition is byte-identical.
    ToolCatalogVisibility,
}

/// One payload-vs-baseline comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixSample {
    /// Percentage of the previous payload's compared bytes that this request
    /// still replays (100 = the system block, the catalog and the whole
    /// message history are unchanged and the history only grew).
    pub stability_pct: u16,
    /// The system part differs from the previous request's.
    pub system_changed: bool,
    /// The tool-catalog part differs from the previous request's.
    pub tools_changed: bool,
    /// A real divergence happened: a part the provider would have replayed
    /// moved, or the model (and with it the cache key) changed.
    pub diverged: bool,
    /// Set when the divergence is one the runtime intends; the cache-miss
    /// estimate is withheld for it.
    pub attributed: Option<Whitelist>,
    /// Rough tokens that must be re-processed after the divergence point.
    /// Zero while the prefix is intact or the divergence is whitelisted.
    pub reprocessed_tokens: u64,
}

/// The provider-request shape families the gate reads system/tools out of.
/// Selection logic lives here (the gate is a host extension, not kernel
/// code); the kernel stays shape-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireShape {
    Anthropic,
    OpenaiResponses,
    OpenaiCompletions,
}

fn wire_shape(api: &str) -> WireShape {
    match api {
        "openai_responses" => WireShape::OpenaiResponses,
        "openai_completions" => WireShape::OpenaiCompletions,
        // Unknown/other shapes are read anthropic-style: the fields the
        // gate looks for (`system`, `tools`, `messages`) are the common
        // denominator, and a miss reads as "unchanged", never as a false
        // cache regression.
        _ => WireShape::Anthropic,
    }
}

/// The compared parts of one payload, split the way a prompt cache replays
/// them: the system block, the tool catalog, and the message array item by
/// item. Keeping the parts apart (instead of the serialized whole object) is
/// what lets a pure append read as an append, because the encoder orders
/// object keys and a shifted byte is not a moved byte.
#[derive(Debug, Clone, Default)]
struct PayloadView {
    /// Model id, for the model-switch whitelist entry.
    model: String,
    /// The system part verbatim: anthropic `system`, responses
    /// `instructions`, or the completions system entries of `messages`.
    /// Compared as JSON, so any move of it is a change.
    system_raw: serde_json::Value,
    /// The same system part flattened to text, for the volatile-line
    /// normalization the whitelist reads.
    system_text: String,
    /// The tool catalog, verbatim (definitions kept in full).
    tools: serde_json::Value,
    /// The conversation history, one `serde_json` encoding per item, in wire
    /// order. The item count doubles as the compaction signal.
    messages: Vec<Vec<u8>>,
}

fn view_of(api: &str, payload: &serde_json::Value) -> PayloadView {
    let shape = wire_shape(api);
    let items: Vec<serde_json::Value> = match shape {
        WireShape::OpenaiResponses => payload.get("input").and_then(|v| v.as_array()),
        _ => payload.get("messages").and_then(|v| v.as_array()),
    }
    .cloned()
    .unwrap_or_default();
    let (system_raw, system_text, history) = match shape {
        WireShape::Anthropic => {
            let system = payload.get("system").cloned().unwrap_or_default();
            let text = blocks_to_text(Some(&system));
            (system, text, items)
        }
        WireShape::OpenaiResponses => {
            let system = payload.get("instructions").cloned().unwrap_or_default();
            let text = system.as_str().unwrap_or_default().to_string();
            (system, text, items)
        }
        // The completions wire carries its system prompt inside the message
        // array. Pulling those entries out keeps the compared parts disjoint,
        // so a turn appended to the history reads as an append and not as a
        // system rewrite.
        WireShape::OpenaiCompletions => {
            let (entries, history): (Vec<serde_json::Value>, Vec<serde_json::Value>) =
                items.into_iter().partition(is_system_message);
            let text = system_text_of(&entries);
            (serde_json::Value::Array(entries), text, history)
        }
    };
    PayloadView {
        model: payload
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        system_raw,
        system_text,
        tools: payload.get("tools").cloned().unwrap_or_default(),
        messages: history
            .iter()
            .filter_map(|m| serde_json::to_vec(m).ok())
            .collect(),
    }
}

/// A completions-wire message that carries the system prompt rather than the
/// conversation (`system`, or its `developer` successor).
fn is_system_message(m: &serde_json::Value) -> bool {
    matches!(
        m.get("role").and_then(|r| r.as_str()),
        Some("system") | Some("developer")
    )
}

/// The completions system text: the `content` of every system entry, joined.
fn system_text_of(entries: &[serde_json::Value]) -> String {
    entries
        .iter()
        .map(|m| blocks_to_text(m.get("content")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Flatten a `string | block[] | {content}` field into plain text.
fn blocks_to_text(value: Option<&serde_json::Value>) -> String {
    match value {
        None => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string(),
    }
}

/// The model-facing name of a catalog entry, in either wire nesting
/// (anthropic/responses inline `name`; completions under `function`).
fn tool_name(entry: &serde_json::Value) -> Option<&str> {
    entry.get("name").and_then(|n| n.as_str()).or_else(|| {
        entry
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
    })
}

/// The catalog as `name -> definition bytes`, with the `cache_control`
/// breakpoint stripped: the marker rides the last entry, so a catalog that
/// grows or shrinks moves it, and a moved marker is not a content change.
fn catalog_by_name(tools: &serde_json::Value) -> HashMap<String, serde_json::Value> {
    let Some(items) = tools.as_array() else {
        return HashMap::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let name = tool_name(item)?.to_string();
            let mut def = item.clone();
            if let Some(map) = def.as_object_mut() {
                map.remove("cache_control");
            }
            Some((name, def))
        })
        .collect()
}

/// True when the catalog moved only by entries entering or leaving: the
/// name sets differ and every retained name keeps its exact definition.
/// A rewritten shared definition is content drift, not visibility.
fn visibility_only_change(prev: &serde_json::Value, cur: &serde_json::Value) -> bool {
    let prev = catalog_by_name(prev);
    let cur = catalog_by_name(cur);
    let mut names_differ = false;
    for (name, def) in &prev {
        match cur.get(name) {
            None => names_differ = true,
            Some(cur_def) if cur_def != def => return false,
            Some(_) => {}
        }
    }
    names_differ || cur.keys().any(|name| !prev.contains_key(name))
}

/// The volatile lines the runtime legitimately re-renders between
/// requests, folded away so a pure-prose system change still reads as a
/// divergence. Redaction is line-scoped and deterministic: a `today`
/// date, the LSP-ready section, and any permission-mode line.
fn normalize_system(system: &str) -> String {
    let mut out = String::new();
    let mut in_lsp_section = false;
    for line in system.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("## LSP ready") {
            in_lsp_section = true;
            continue;
        }
        if in_lsp_section {
            if trimmed.starts_with("## ") || trimmed.starts_with("# ") {
                in_lsp_section = false;
            } else {
                continue;
            }
        }
        if trimmed.starts_with("Date:") || trimmed.to_ascii_lowercase().contains("permission mode")
        {
            continue;
        }
        out.push_str(&redact_dates(line));
        out.push('\n');
    }
    out
}

/// Replace every `YYYY-MM-DD` date with a fixed marker (hand-rolled to keep
/// the gate dependency-free).
fn redact_dates(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.len() >= 10 && is_date(rest) && rest[4] == b'-' && rest[7] == b'-' {
            out.push_str("<date>");
            i += 10;
            continue;
        }
        // Safe: slicing at a byte boundary of an ASCII digit run keeps the
        // remainder on a char boundary because the consumed bytes are ASCII.
        out.push(line[i..].chars().next().unwrap());
        i += line[i..].chars().next().unwrap().len_utf8();
    }
    out
}

/// A `YYYY-MM-DD` date literal at the head of a byte slice.
fn is_date(bytes: &[u8]) -> bool {
    bytes.len() >= 10
        && bytes[..10].iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        })
}

/// The wire byte length of one JSON part (missing bytes price as nothing
/// reused, never as a false cache regression).
fn json_len(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value)
        .map(|b| b.len())
        .unwrap_or_default()
}

/// Leading history items whose encoded bytes are equal: the part of the
/// history a prompt cache can replay.
fn common_history_len(prev: &[Vec<u8>], cur: &[Vec<u8>]) -> usize {
    prev.iter()
        .zip(cur.iter())
        .take_while(|(p, c)| p == c)
        .count()
}

/// Compare one payload against the previous snapshot. Pure, so the gate's
/// arithmetic is testable without a channel or a session.
fn classify(prev: &PayloadView, cur: &PayloadView) -> PrefixSample {
    let model_changed = prev.model != cur.model;
    let system_changed = prev.system_raw != cur.system_raw;
    let tools_changed = prev.tools != cur.tools;
    let common = common_history_len(&prev.messages, &cur.messages);
    let history_prefix = common == prev.messages.len();
    // A model change moves the cache key even when every compared part is
    // identical, so it is a divergence as well (and is whitelisted below).
    let diverged = model_changed || system_changed || tools_changed || !history_prefix;

    let system_bytes = json_len(&prev.system_raw);
    let tools_bytes = json_len(&prev.tools);
    let history_bytes: usize = prev.messages.iter().map(|m| m.len()).sum();
    let prev_total_bytes = system_bytes + tools_bytes + history_bytes;
    let reused_bytes = if system_changed { 0 } else { system_bytes }
        + if tools_changed { 0 } else { tools_bytes }
        + prev.messages[..common]
            .iter()
            .map(|m| m.len())
            .sum::<usize>();
    let stability_pct = if prev_total_bytes == 0 {
        100
    } else {
        (((reused_bytes as u64 * 100) / prev_total_bytes as u64).min(100)) as u16
    };

    let mut attributed: Option<Whitelist> = None;
    if diverged {
        if model_changed {
            attributed = Some(Whitelist::ModelSwitch);
        } else if cur.messages.len() < prev.messages.len() {
            attributed = Some(Whitelist::Compaction);
        } else if system_changed
            && normalize_system(&prev.system_text) == normalize_system(&cur.system_text)
        {
            // The system text moved only through the volatile lines.
            attributed = Some(
                if prev.system_text.contains("## LSP ready")
                    != cur.system_text.contains("## LSP ready")
                {
                    Whitelist::LspReadyLine
                } else if prev.system_text.contains("permission mode")
                    || cur.system_text.contains("permission mode")
                {
                    Whitelist::PermissionMode
                } else {
                    Whitelist::TodayRollover
                },
            );
        } else if tools_changed
            && !system_changed
            && visibility_only_change(&prev.tools, &cur.tools)
        {
            attributed = Some(Whitelist::ToolCatalogVisibility);
        }
    }

    let reprocessed_tokens = if diverged && attributed.is_none() {
        ((prev_total_bytes - reused_bytes) / APPROX_BYTES_PER_TOKEN) as u64
    } else {
        0
    };

    PrefixSample {
        stability_pct,
        system_changed,
        tools_changed,
        diverged,
        attributed,
        reprocessed_tokens,
    }
}

/// The host-side provider-request observer: compares each wire payload
/// with the previous one of this run and publishes the result as
/// `ThreadEvent`s on the engine's notice channel.
pub struct PrefixStabilityGate {
    thread_id: String,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    state: Mutex<Option<PayloadView>>,
}

impl PrefixStabilityGate {
    pub fn new(
        thread_id: impl Into<String>,
        notice_tx: mpsc::UnboundedSender<BackendNotice>,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            notice_tx,
            state: Mutex::new(None),
        }
    }

    /// The thread this gate reports for.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Record one payload and publish the comparison against this run's
    /// baseline. Returns `None` for the first payload of a run (nothing to
    /// compare against yet) and leaves the baseline of a poisoned gate alone.
    pub fn observe(&self, api: &str, payload: &serde_json::Value) -> Option<PrefixSample> {
        let view = view_of(api, payload);
        let sample = {
            let mut guard = self.state.lock().ok()?;
            let sample = guard.as_ref().map(|prev| classify(prev, &view));
            *guard = Some(view);
            sample
        };
        let sample = sample?;
        let _ = self.notice_tx.send(BackendNotice::Event(Box::new(
            ThreadEvent::PrefixStability {
                stability_pct: sample.stability_pct,
                system_changed: sample.system_changed,
                tools_changed: sample.tools_changed,
            },
        )));
        if sample.diverged && sample.attributed.is_none() {
            let _ = self.notice_tx.send(BackendNotice::Event(Box::new(
                ThreadEvent::CacheInvalidation {
                    reprocessed_tokens: sample.reprocessed_tokens,
                },
            )));
        }
        Some(sample)
    }

    /// Forget the baseline, as a new run does.
    pub fn reset(&self) {
        if let Ok(mut guard) = self.state.lock() {
            *guard = None;
        }
    }

    /// The `BeforeProviderPayload` hook handler that drives this gate on
    /// the production stream path. Read-only by construction: the context
    /// comes back untouched, so the payload the kernel sends is the
    /// payload the provider receives.
    pub fn hook_handler(gate: Arc<PrefixStabilityGate>) -> manox_harness::harness::HookHandler {
        Arc::new(move |ctx| {
            let api = ctx
                .data
                .get("model")
                .and_then(|m| m.get("api"))
                .and_then(|a| a.as_str())
                .unwrap_or_default();
            if let Some(payload) = ctx.data.get("payload") {
                let _ = gate.observe(api, payload);
            }
            ctx
        })
    }
}

impl RequestObserver for PrefixStabilityGate {
    fn before_payload(
        &self,
        attempt: u32,
        model: &manox_harness::types::Model,
        payload: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        // Retries resend the same bytes; only the first attempt of a
        // request advances the baseline.
        if attempt == 1 {
            let _ = self.observe(&model.api, payload);
        }
        // Observation never substitutes a payload.
        None
    }

    fn after_response(&self, _attempt: u32, _status: u16, _headers: &reqwest::header::HeaderMap) {}
}

/// Attach the stability gate to a session: registers the
/// `BeforeProviderPayload` handler on the harness hook the kernel's bridged
/// [`RequestObserver`] already fires for every wire attempt of the
/// production resolver, so the gate sees real provider traffic without
/// replacing the stream function.
pub fn attach_prefix_gate(
    session: &mut manox_harness::coding_agent::AgentSession,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    thread_id: &str,
) {
    let gate = Arc::new(PrefixStabilityGate::new(thread_id, notice_tx.clone()));
    session.on(
        manox_harness::harness::HookPoint::BeforeProviderPayload,
        PrefixStabilityGate::hook_handler(gate),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_payload(
        model: &str,
        system: &str,
        tools: &[&str],
        messages: &[&str],
    ) -> serde_json::Value {
        serde_json::json!({
            "model": model,
            "system": [{ "type": "text", "text": system }],
            "tools": tools.iter().map(|t| serde_json::json!({
                "name": t, "description": format!("{t} desc"),
                "input_schema": { "type": "object" }
            })).collect::<Vec<_>>(),
            "messages": messages.iter().map(|m| serde_json::json!({
                "role": "user", "content": [{ "type": "text", "text": m }]
            })).collect::<Vec<_>>(),
            "max_tokens": 1024,
            "stream": true,
        })
    }

    fn gate() -> (PrefixStabilityGate, mpsc::UnboundedReceiver<BackendNotice>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (PrefixStabilityGate::new("thread-1", tx), rx)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<BackendNotice>) -> Vec<ThreadEvent> {
        let mut out = Vec::new();
        while let Ok(BackendNotice::Event(ev)) = rx.try_recv() {
            out.push(*ev);
        }
        out
    }

    /// Append-only growth: the previous payload stays a byte-exact prefix,
    /// so the turn reports perfect stability and no cache event.
    #[test]
    fn stabilizer_append_only_growth() {
        let (gate, mut rx) = gate();
        let first = anthropic_payload("m", "SYSTEM", &["Read"], &["hello"]);
        assert!(
            gate.observe("anthropic", &first).is_none(),
            "the first payload only sets the baseline"
        );
        let second = anthropic_payload("m", "SYSTEM", &["Read"], &["hello", "again"]);
        let sample = gate
            .observe("anthropic", &second)
            .expect("a baseline exists");
        assert_eq!(sample.stability_pct, 100);
        assert!(!sample.diverged);
        assert!(!sample.system_changed);
        assert!(!sample.tools_changed);
        assert_eq!(sample.reprocessed_tokens, 0);
        let events = drain(&mut rx);
        assert!(
            matches!(
                events.as_slice(),
                [ThreadEvent::PrefixStability {
                    stability_pct: 100,
                    system_changed: false,
                    tools_changed: false
                }]
            ),
            "growth emits only the stability metric: {events:?}"
        );
    }

    /// A system rewrite diverges inside the prefix and is not whitelisted
    /// when the prose itself moved, so the gate also reports the token
    /// re-processing estimate.
    #[test]
    fn stabilizer_system_change_is_unattributed() {
        let (gate, mut rx) = gate();
        let first = anthropic_payload("m", "SYSTEM", &["Read"], &["hello"]);
        let second = anthropic_payload("m", "SYSTEM EDITED", &["Read"], &["hello"]);
        let sample = {
            gate.observe("anthropic", &first);
            gate.observe("anthropic", &second).expect("sample")
        };
        assert!(sample.system_changed);
        assert!(!sample.tools_changed);
        assert!(sample.diverged);
        assert_eq!(sample.attributed, None);
        assert!(sample.reprocessed_tokens > 0);
        assert!(sample.stability_pct < 100);
        let events = drain(&mut rx);
        assert!(
            matches!(
                events.as_slice(),
                [ThreadEvent::PrefixStability { system_changed: true, .. }, ThreadEvent::CacheInvalidation { reprocessed_tokens }]
                    if *reprocessed_tokens > 0
            ),
            "a real system drift emits both metrics: {events:?}"
        );
    }

    /// A `today` rollover inside the system prompt is whitelisted: the
    /// stability metric still moves, no cache-miss estimate is published.
    #[test]
    fn stabilizer_day_rollover_is_whitelisted() {
        let (gate, mut rx) = gate();
        let first = anthropic_payload("m", "Date: 2026-07-14\nProse.", &["Read"], &["hello"]);
        let second = anthropic_payload("m", "Date: 2026-07-15\nProse.", &["Read"], &["hello"]);
        gate.observe("anthropic", &first);
        let sample = gate.observe("anthropic", &second).expect("sample");
        assert!(sample.system_changed && sample.diverged);
        assert_eq!(sample.attributed, Some(Whitelist::TodayRollover));
        assert_eq!(sample.reprocessed_tokens, 0);
        let events = drain(&mut rx);
        assert!(
            matches!(events.as_slice(), [ThreadEvent::PrefixStability { .. }]),
            "whitelisted divergence reports stability only: {events:?}"
        );
    }

    /// A tool definition rewritten in place is NOT a visibility change.
    #[test]
    fn stabilizer_tools_change_is_unattributed() {
        let (gate, mut rx) = gate();
        let first = anthropic_payload("m", "SYSTEM", &["Read", "Bash"], &["hello"]);
        let second = serde_json::json!({
            "model": "m",
            "system": [{ "type": "text", "text": "SYSTEM" }],
            "tools": [
                { "name": "Read", "description": "REWRITTEN", "input_schema": { "type": "object" } },
                { "name": "Bash", "description": "Bash desc", "input_schema": { "type": "object" } }
            ],
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hello" }] }],
            "max_tokens": 1024,
            "stream": true,
        });
        gate.observe("anthropic", &first);
        let sample = gate.observe("anthropic", &second).expect("sample");
        assert!(sample.tools_changed);
        assert!(!sample.system_changed);
        assert_eq!(sample.attributed, None);
        assert!(sample.reprocessed_tokens > 0);
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e,
            ThreadEvent::PrefixStability {
                tools_changed: true,
                ..
            }
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ThreadEvent::CacheInvalidation { .. }))
        );
    }

    /// Dropping a tool (visibility) is whitelisted; adding one is too.
    #[test]
    fn stabilizer_tool_visibility_change_is_whitelisted() {
        let (gate, _rx) = gate();
        let first = anthropic_payload("m", "SYSTEM", &["Read", "Bash"], &["hello"]);
        let second = anthropic_payload("m", "SYSTEM", &["Read"], &["hello"]);
        gate.observe("anthropic", &first);
        let sample = gate.observe("anthropic", &second).expect("sample");
        assert!(sample.tools_changed);
        assert_eq!(sample.attributed, Some(Whitelist::ToolCatalogVisibility));
        assert_eq!(sample.reprocessed_tokens, 0);
    }

    /// A rewrite in the middle of the history diverges before the end of
    /// the previous prefix and is attributed to the compaction boundary
    /// (the message array shrank); an equal-length rewrite is not.
    #[test]
    fn stabilizer_mid_rewrite() {
        let (gate, mut rx) = gate();
        let first = anthropic_payload("m", "SYSTEM", &["Read"], &["one", "two", "three"]);
        // Same length, middle item rewritten: not a compaction.
        let second = serde_json::json!({
            "model": "m",
            "system": [{ "type": "text", "text": "SYSTEM" }],
            "tools": [{ "name": "Read", "description": "Read desc", "input_schema": { "type": "object" } }],
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "one" }] },
                { "role": "user", "content": [{ "type": "text", "text": "TWO REWRITTEN" }] },
                { "role": "user", "content": [{ "type": "text", "text": "three" }] }
            ],
            "max_tokens": 1024,
            "stream": true,
        });
        gate.observe("anthropic", &first);
        let rewritten = gate.observe("anthropic", &second).expect("sample");
        assert!(rewritten.diverged);
        assert!(rewritten.stability_pct < 100);
        assert_eq!(rewritten.attributed, None);
        assert!(rewritten.reprocessed_tokens > 0);
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ThreadEvent::CacheInvalidation { .. }))
        );

        // Shorter history: the compaction boundary owns the divergence.
        let compacted = anthropic_payload("m", "SYSTEM", &["Read"], &["summary"]);
        let sample = gate.observe("anthropic", &compacted).expect("sample");
        assert!(sample.diverged);
        assert_eq!(sample.attributed, Some(Whitelist::Compaction));
        assert_eq!(sample.reprocessed_tokens, 0);
    }

    /// A model switch is whitelisted (the cache key moves with it).
    #[test]
    fn stabilizer_model_switch_is_whitelisted() {
        let (gate, _rx) = gate();
        gate.observe("anthropic", &anthropic_payload("a", "S", &["Read"], &["x"]));
        let sample = gate
            .observe("anthropic", &anthropic_payload("b", "S", &["Read"], &["x"]))
            .expect("sample");
        assert_eq!(sample.attributed, Some(Whitelist::ModelSwitch));
        assert_eq!(sample.reprocessed_tokens, 0);
    }

    /// The `openai_completions` system lives inside `messages`; the gate
    /// must read it there (and not mistake the appended history for a
    /// system change).
    #[test]
    fn stabilizer_reads_the_completions_system_entry() {
        let (gate, _rx) = gate();
        let first = serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                { "role": "system", "content": "SYS" },
                { "role": "user", "content": "hi" }
            ],
            "tools": [{ "type": "function", "function": { "name": "Read", "description": "d", "parameters": {} } }],
            "stream": true
        });
        let second = serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                { "role": "system", "content": "SYS" },
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "yo" }
            ],
            "tools": [{ "type": "function", "function": { "name": "Read", "description": "d", "parameters": {} } }],
            "stream": true
        });
        gate.observe("openai_completions", &first);
        let sample = gate.observe("openai_completions", &second).expect("sample");
        assert!(!sample.system_changed);
        assert!(!sample.tools_changed);
        assert_eq!(sample.stability_pct, 100);
    }

    /// The responses shape keeps its system text in `instructions`.
    #[test]
    fn stabilizer_reads_the_responses_instructions() {
        let (gate, _rx) = gate();
        let mk = |instr: &str| {
            serde_json::json!({
                "model": "gpt-5",
                "instructions": instr,
                "input": [{ "role": "user", "content": "hi" }],
                "tools": [{ "type": "function", "name": "Read", "description": "d", "parameters": {} }]
            })
        };
        gate.observe("openai_responses", &mk("A"));
        let sample = gate.observe("openai_responses", &mk("B")).expect("sample");
        assert!(sample.system_changed);
    }

    #[test]
    fn date_redaction_and_shape_reading() {
        assert_eq!(
            redact_dates("Date: 2026-07-14 — utf8 ✓"),
            "Date: <date> — utf8 ✓"
        );
        let shape_cases = [
            ("anthropic", WireShape::Anthropic),
            ("openai_responses", WireShape::OpenaiResponses),
            ("openai_completions", WireShape::OpenaiCompletions),
            ("something_else", WireShape::Anthropic),
        ];
        for (api, want) in shape_cases {
            assert_eq!(wire_shape(api), want, "api {api}");
        }
        assert!(catalog_by_name(&serde_json::Value::Null).is_empty());
        assert!(visibility_only_change(
            &serde_json::json!([{"name": "Read"}]),
            &serde_json::json!([{"name": "Read"}, {"name": "Bash"}])
        ));
        assert!(!visibility_only_change(
            &serde_json::json!([{"name": "Read", "description": "a"}]),
            &serde_json::json!([{"name": "Read", "description": "b"}])
        ));
    }

    /// The gate is read-only on the hook path: whatever the handler gets,
    /// it hands back unchanged.
    #[test]
    fn hook_handler_never_mutates() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let gate = Arc::new(PrefixStabilityGate::new("t", tx));
        let handler = PrefixStabilityGate::hook_handler(Arc::clone(&gate));
        let ctx = manox_harness::harness::HookContext::new(
            manox_harness::harness::HookPoint::BeforeProviderPayload,
        )
        .with_data(serde_json::json!({
            "attempt": 1,
            "model": { "provider": "anthropic", "id": "m", "api": "anthropic" },
            "payload": anthropic_payload("m", "S", &["Read"], &["x"])
        }));
        let payload_in = ctx.data["payload"].clone();
        let out = handler(ctx);
        assert_eq!(
            out.data["payload"], payload_in,
            "the payload is returned untouched"
        );
        // The observer path is read-only too: `before_payload` never
        // substitutes.
        let model = manox_harness::types::Model {
            provider: "anthropic".into(),
            api: "anthropic".into(),
            id: "m".into(),
            context_window: 200_000,
            max_tokens: 8_192,
            thinking: manox_harness::types::ThinkingKind::None,
            metadata: Default::default(),
        };
        assert!(
            gate.before_payload(1, &model, &payload_in).is_none(),
            "observation never replaces the payload"
        );
    }

    /// The gate prices only the part that moved: a system rewrite leaves the
    /// catalog and the whole (extended) history reusable, so both the reported
    /// stability and the re-processing estimate cover the system bytes alone.
    #[test]
    fn stabilizer_prices_only_the_diverged_part() {
        let prev = anthropic_payload("m", "SYSTEM", &["Read"], &["one", "two"]);
        let cur = anthropic_payload("m", "SYSTEM EDITED", &["Read"], &["one", "two", "three"]);
        let sample = classify(&view_of("anthropic", &prev), &view_of("anthropic", &cur));
        let system_bytes = serde_json::to_vec(&prev["system"]).unwrap().len();
        let tools_bytes = serde_json::to_vec(&prev["tools"]).unwrap().len();
        let history_bytes: usize = prev["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| serde_json::to_vec(m).unwrap().len())
            .sum();
        let total_bytes = system_bytes + tools_bytes + history_bytes;
        assert!(sample.diverged && sample.system_changed);
        assert!(!sample.tools_changed);
        assert_eq!(sample.attributed, None);
        assert_eq!(
            sample.reprocessed_tokens,
            (system_bytes / APPROX_BYTES_PER_TOKEN) as u64,
            "only the system block counts as re-processed"
        );
        assert_eq!(
            sample.stability_pct,
            (((total_bytes - system_bytes) as u64 * 100) / total_bytes as u64) as u16,
            "the catalog and the history are still replayed"
        );
        assert!(sample.stability_pct > 0 && sample.stability_pct < 100);
    }

    /// A pure append on the responses wire (history in `input`) is the steady
    /// state of a run: perfect stability, no divergence, no cache event.
    #[test]
    fn stabilizer_responses_append_is_perfectly_stable() {
        let (gate, mut rx) = gate();
        let mk = |extra: &[&str]| {
            let mut input: Vec<serde_json::Value> =
                vec![serde_json::json!({ "role": "user", "content": "hi" })];
            input.extend(
                extra
                    .iter()
                    .map(|t| serde_json::json!({ "role": "assistant", "content": t })),
            );
            serde_json::json!({
                "model": "gpt-5",
                "instructions": "SYS",
                "input": input,
                "tools": [{ "type": "function", "name": "Read", "description": "d", "parameters": {} }]
            })
        };
        gate.observe("openai_responses", &mk(&[]));
        let sample = gate
            .observe("openai_responses", &mk(&["yo"]))
            .expect("sample");
        assert_eq!(sample.stability_pct, 100);
        assert!(!sample.diverged);
        assert_eq!(sample.attributed, None);
        assert_eq!(sample.reprocessed_tokens, 0);
        let events = drain(&mut rx);
        assert!(
            matches!(
                events.as_slice(),
                [ThreadEvent::PrefixStability {
                    stability_pct: 100,
                    system_changed: false,
                    tools_changed: false
                }]
            ),
            "an append emits only the stability metric: {events:?}"
        );
    }

    /// `reset` drops the baseline, so swapping the session chain on one gate
    /// never reports a stale divergence against the previous run's bytes.
    #[test]
    fn reset_drops_the_baseline() {
        let (gate, mut rx) = gate();
        gate.observe("anthropic", &anthropic_payload("m", "S", &["Read"], &["x"]));
        gate.reset();
        let rewritten = anthropic_payload("m", "OTHER", &["Bash"], &["y", "z"]);
        assert!(
            gate.observe("anthropic", &rewritten).is_none(),
            "the first payload after a reset only re-seeds the baseline"
        );
        drain(&mut rx);
    }
}
