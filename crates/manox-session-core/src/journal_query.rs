//! Journal read services on the gateway: `PageHistory` (cold chain page,
//! §D.2) and `GetConversationInfo` (the §E.3 Q-face fold), T4.
//!
//! Both ride the kernel journal read seam (§C.3, `ThreadHandle::
//! journal_snapshot`): a whole active-chain read answered by the engine
//! actor. "Cold" here means the page fold never starts a provider turn and
//! never touches the engine's live transcript mirror — it is a pure chain
//! read (§D.2). `PageHistory` serves the §F.1 gap-repair and backwards
//! paging pages; `GetConversationInfo` folds turns / messages / per-model
//! usage (§E.3) and is cached by `(thread_id, cursor)` — recomputed only
//! when the cursor advances.

use std::collections::HashMap;
use std::sync::Arc;

use manox_agent::thread::ThreadHandle;
use manox_protocol::journal::{JournalWireEntry, ModelRef};
use serde_json::{Value, json};

use crate::translate::wire_entry;

/// `(thread_id, cursor) → folded payload` (§E.3 cache).
#[derive(Default)]
pub struct ConversationInfoCache {
    map: HashMap<(String, u64), Value>,
}

impl ConversationInfoCache {
    fn get(&self, thread_id: &str, cursor: u64) -> Option<&Value> {
        self.map.get(&(thread_id.to_string(), cursor))
    }
    fn put(&mut self, thread_id: &str, cursor: u64, value: Value) {
        // Bound the cache to the sessions currently served: keep at most
        // one entry per thread (the previous cursor's fold is dead once the
        // journal moves).
        self.map.retain(|(t, _), _| t != thread_id);
        self.map.insert((thread_id.to_string(), cursor), value);
    }
}

/// `ClientCall::PageHistory` (§D.2): `{records, has_more, cursor}`.
///
/// `through_seq` is the inclusive tail (`-1` = latest); `before_seq` is an
/// exclusive upper bound for backwards paging; `max_messages` caps the page
/// from the tail. The returned `records` are the §C.1 wire entries of the
/// active chain slice — dense, oldest-first — and `cursor` is the tail seq
/// of the page (the §F.1 repair contract: a non-empty page ends at its
/// cursor). Kernel rows with no §C.2 wire vocabulary (`ActiveToolsChange`,
/// `Custom`, `CustomMessage`) are skipped and do not open gaps (§F.1 rule 2
/// tolerates unclaimed seqs).
pub async fn page_history(
    thread: &ThreadHandle,
    through_seq: i64,
    before_seq: Option<i64>,
    max_messages: Option<u32>,
) -> Result<Value, manox_protocol::RpcError> {
    let snapshot = thread.journal_snapshot().await.ok_or_else(|| {
        manox_protocol::RpcError::new(-1, "journal engine is not materialized")
            .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)
    })?;
    // Inclusive upper bound of the requested window.
    let through = if through_seq < 0 {
        snapshot.cursor
    } else {
        (through_seq as u64).min(snapshot.cursor)
    };
    let through = match before_seq {
        Some(b) if b > 0 => through.min((b as u64).saturating_sub(1)),
        // `before_seq <= 0` asks for entries strictly before the root / an
        // inverted window: empty page, cursor pinned at the bound.
        Some(_) => return Ok(json!({ "records": [], "has_more": false, "cursor": through })),
        None => through,
    };
    let mut records: Vec<JournalWireEntry> = snapshot
        .records
        .iter()
        .filter(|r| r.seq <= through)
        .filter_map(|r| wire_entry(r.seq, &r.entry))
        .collect();
    let window = match max_messages {
        Some(n) if (records.len() as u32) > n => {
            let start = records.len() - n as usize;
            records.split_off(start)
        }
        _ => std::mem::take(&mut records),
    };
    let has_more =
        !window.is_empty() && (window.first().is_some_and(|r| r.seq > 0) || (!records.is_empty()));
    let cursor = window.last().map(|r| r.seq).unwrap_or(through);
    Ok(json!({
        "records": window,
        "has_more": has_more,
        "cursor": cursor,
    }))
}

/// `ClientCall::GetConversationInfo` (§E.3, Q face): the server-side fold of
/// the journal, cached by `(thread_id, cursor)` — recomputed only when the
/// cursor advances.
///
/// Field sourcing (best effort per §E.3; missing → `null`):
/// - `turns` = `turn_start` entry count; `messages` = `message` entry count;
/// - `models[]` aggregates assistant messages by `{provider}/{model}` with
///   per-request usage (`input/output/cacheRead/cacheWrite/reasoning`),
///   `calls`, `lastTotal` (last request's total context tokens);
/// - `contextWindow` / `hitRate` / `pct` are token-meter semantics that need
///   the provider registry + cache accounting beyond the journal — `null`
///   in T4 (T5 projection/registry work);
/// - `cumulativeCost` = 0.0 placeholder (real cost folding is T5);
/// - `git` = null placeholder (git stats stay a host lookup, §E.3 note).
pub async fn conversation_info(
    cache: &Arc<std::sync::Mutex<ConversationInfoCache>>,
    thread: &ThreadHandle,
    session_id: &str,
) -> Result<Value, manox_protocol::RpcError> {
    let snapshot = thread.journal_snapshot().await.ok_or_else(|| {
        manox_protocol::RpcError::new(-1, "journal engine is not materialized")
            .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)
    })?;
    {
        let guard = cache.lock().unwrap();
        if let Some(hit) = guard.get(session_id, snapshot.cursor) {
            return Ok(hit.clone());
        }
    }
    let folded = fold_conversation_info(thread, session_id, &snapshot.records, snapshot.cursor);
    cache
        .lock()
        .unwrap()
        .put(session_id, snapshot.cursor, folded.clone());
    Ok(folded)
}

/// The pure fold over one chain read (split out so tests can drive it
/// directly). `records` are dense seq-ordered kernel chain positions.
fn fold_conversation_info(
    thread: &ThreadHandle,
    session_id: &str,
    records: &[manox_harness::session::jsonl::JournalRecord],
    cursor: u64,
) -> Value {
    use manox_harness::session::SessionTreeEntry as E;
    use manox_harness::types::AgentMessage as M;

    let mut turns: u64 = 0;
    let mut messages: u64 = 0;
    // (provider, model) → aggregate.
    let mut models: HashMap<(String, String), ModelAgg> = HashMap::new();
    let mut title: Option<String> = None;
    for record in records {
        match &record.entry {
            E::TurnStart { .. } => turns += 1,
            E::Title { title: t, .. } => title = Some(t.clone()),
            E::Message { message, .. } => {
                messages += 1;
                if let M::Assistant {
                    provider,
                    model,
                    usage,
                    ..
                } = message
                {
                    let agg = models.entry((provider.clone(), model.clone())).or_default();
                    agg.input += usage.input_tokens;
                    agg.output += usage.output_tokens;
                    agg.cache_read += usage.cache_read_input_tokens;
                    agg.cache_write += usage.cache_creation_input_tokens;
                    agg.reasoning += usage.reasoning_tokens.unwrap_or(0);
                    agg.calls += 1;
                    // tokenMeter semantics: the last request's full context
                    // numerator (input incl. cache classes + output).
                    agg.last_total = usage
                        .total_tokens
                        .max(usage.total_input() + usage.output_tokens);
                }
            }
            _ => {}
        }
    }
    let mut model_rows: Vec<Value> = models
        .into_iter()
        .map(|((provider, model), agg)| {
            json!({
                "provider": provider,
                // Canonical wire identity (L8): `{provider}/{model}`.
                "model": ModelRef::new(format!("{provider}/{model}")).0,
                "input": agg.input,
                "output": agg.output,
                "cacheRead": agg.cache_read,
                "cacheWrite": agg.cache_write,
                "reasoning": agg.reasoning,
                "calls": agg.calls,
                "lastTotal": agg.last_total,
                // Token-meter fields need the provider registry (T5): null.
                "contextWindow": Value::Null,
                "hitRate": Value::Null,
                "pct": Value::Null,
            })
        })
        .collect();
    model_rows.sort_by_key(|row| {
        (
            row["provider"].as_str().unwrap_or("").to_string(),
            row["model"].as_str().unwrap_or("").to_string(),
        )
    });
    let display_title = thread.read(|t| t.display_title());
    json!({
        "threadId": session_id,
        "cursor": cursor,
        "title": title.or(Some(display_title)),
        "cwd": thread.read(|t| t.cwd().to_string_lossy().into_owned()),
        "project": thread.read(|t| t.project().map(|p| p.to_string_lossy().into_owned())),
        "model": thread.read(|t| t.model().map(|m| format!("{}/{}", m.provider, m.id))),
        "contextWindow": thread.read(|t| t.model().map(|m| m.context_window)),
        "turns": turns,
        "messages": messages,
        "models": model_rows,
        // Real cost folding is T5 (§E.3); 0.0 placeholder keeps the row
        // shape stable for clients.
        "cumulativeCost": 0.0,
        // Git stats are a host lookup (§E.3 note); null placeholder in T4.
        "git": Value::Null,
    })
}

/// Per-model usage aggregate (the §E.3 `models[]` row).
#[derive(Default, Clone, Copy)]
struct ModelAgg {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
    calls: u64,
    last_total: u64,
}
