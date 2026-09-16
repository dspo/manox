//! Durable projection checkpoints (deepseek-harness session-projection-
//! cache parity, simplified): one json record per session binding the rows
//! to the log identity they were folded from.
//!
//! A record is a fold shortcut, never an authority: an identity mismatch
//! (recreated session id, swapped home, format bump) or a row watermark
//! past the supplied cursor discards the WHOLE record — the caller refolds
//! from the live seed. Writes are fail-soft (a lost checkpoint costs one
//! longer tail fold).
//!
//! Identity choice (documented decision): the journal file's header line
//! (immutable per chain). A session without a materialized journal has no
//! cache-worthy fold, so absence disables both save and load.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::projections::ProjectionSet;

const FORMAT_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    /// A stale key set (a build with fewer declared keys) must discard the
    /// whole record instead of seeding a partial fold.
    #[test]
    fn load_discards_records_with_a_stale_key_set() {
        let _g = crate::test_support::lock_globals_for_cache_test();
        manox_agent::runtime::hermetic_home_for_test();
        manox_agent::runtime::init();
        manox_agent::thread_store::drop_global_for_test();
        manox_agent::thread_store::init();
        // A materialized chain header is the cache identity; without it
        // both save and load are inert.
        let sessions = manox_agent::paths::manox_config_dir()
            .unwrap()
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("c-1.jsonl"),
            "{\"type\":\"session\",\"timestamp\":\"2026-01-01T00:00:00Z\"}\n",
        )
        .unwrap();
        let rows: BTreeMap<String, (u64, JsonValue)> = manox_protocol::surface::PROJECTION_KEYS
            .iter()
            .skip(1)
            .map(|key| (key.to_string(), (0, JsonValue::Null)))
            .collect();
        save("c-1", &rows);
        assert!(
            load("c-1", 0).is_none(),
            "a record missing declared keys must refold, never seed"
        );
        // A row watermark past the supplied cursor is equally unusable.
        let full: BTreeMap<String, (u64, JsonValue)> = manox_protocol::surface::PROJECTION_KEYS
            .iter()
            .map(|key| (key.to_string(), (4, JsonValue::Null)))
            .collect();
        save("c-1", &full);
        assert!(load("c-1", 3).is_none(), "rows past the cursor must refold");
        assert!(load("c-1", 4).is_some(), "a bounded, complete record loads");
    }
}

#[derive(Serialize, Deserialize)]
struct Row {
    ver: u32,
    seq: i64,
    val: JsonValue,
}

#[derive(Serialize, Deserialize)]
struct Record {
    identity: Identity,
    rows: BTreeMap<String, Row>,
}

#[derive(Serialize, Deserialize)]
struct Identity {
    format_version: u32,
    chain_header: String,
}

fn cache_dir() -> Option<PathBuf> {
    manox_agent::paths::manox_config_dir()
        .ok()
        .map(|dir| dir.join("projection_cache"))
}

fn record_path(session_id: &str) -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join(format!("{session_id}.json")))
}

/// The chain identity a checkpoint binds to: the journal header's creation
/// timestamp (immutable per chain, unlike list mirrors that reconcile away
/// for unmaterized sessions). A session without a materialized journal has
/// no cache-worthy fold, so absence disables both save and load.
fn chain_identity(session_id: &str) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let path = manox_agent::paths::manox_config_dir()
        .ok()?
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let has = line.contains("\"session\"");
    has.then_some(line)
}

/// Load a usable checkpoint: identity-bound and watermark-bounded against
/// the supplied journal cursor.
pub fn load(session_id: &str, cursor: u64) -> Option<(ProjectionSet, Option<u64>)> {
    let path = record_path(session_id)?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let record: Record = serde_json::from_str(&raw).ok()?;
    if record.identity.format_version != FORMAT_VERSION {
        return None;
    }
    if Some(&record.identity.chain_header) != chain_identity(session_id).as_ref() {
        tracing::debug!(session = %session_id, "projection cache identity mismatch; refolding");
        return None;
    }
    if record.rows.keys().len() != manox_protocol::surface::PROJECTION_KEYS.len()
        || !manox_protocol::surface::PROJECTION_KEYS
            .iter()
            .all(|key| record.rows.contains_key(*key))
    {
        tracing::debug!(session = %session_id, "projection cache key set stale; refolding");
        return None;
    }
    let mut rows = BTreeMap::new();
    let mut observed: Option<u64> = None;
    for (key, row) in &record.rows {
        if row.ver != FORMAT_VERSION || row.seq > cursor as i64 {
            tracing::debug!(session = %session_id, %key, "projection cache row unusable; refolding");
            return None;
        }
        let seq = if row.seq < 0 { 0 } else { row.seq as u64 };
        observed = Some(observed.map_or(seq, |last: u64| last.max(seq)));
        rows.insert(key.clone(), (seq, row.val.clone()));
    }
    Some((ProjectionSet::from_checkpoint(rows), observed))
}

/// Fail-soft durable write of one session's whole cut.
pub fn save(session_id: &str, rows: &BTreeMap<String, (u64, JsonValue)>) {
    let Some(path) = record_path(session_id) else {
        return;
    };
    let Some(chain_header) = chain_identity(session_id) else {
        return;
    };
    let record = Record {
        identity: Identity {
            format_version: FORMAT_VERSION,
            chain_header,
        },
        rows: rows
            .iter()
            .map(|(key, (seq, val))| {
                (
                    key.clone(),
                    Row {
                        ver: FORMAT_VERSION,
                        seq: *seq as i64,
                        val: val.clone(),
                    },
                )
            })
            .collect(),
    };
    let Ok(raw) = serde_json::to_vec(&record) else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(path.parent().expect("cache path has a parent")) {
        tracing::debug!(%error, "projection cache dir failed");
        return;
    }
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::write(&tmp, raw).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(error) = written {
        tracing::debug!(%error, "projection cache write failed (fail-soft)");
        let _ = std::fs::remove_file(tmp);
    }
}
