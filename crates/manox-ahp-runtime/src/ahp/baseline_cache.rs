//! Process-lifetime cache for thread-scoped extension baselines.
//!
//! [`super::extension_channel_baseline`] folds every member journal of the
//! owning thread on every subscribe; a reconnect storm on a long
//! multi-member thread pays repeated full-journal IO for a result that can
//! only change when one of those journals moves. The cache keys on
//! `(sessions dir, thread, channel)` and validates against a fingerprint of
//! the fold's exact inputs — which member is active, the member set, and
//! each member journal's on-disk stamp (length + mtime).
//!
//! A hit is therefore the fold of unchanged bytes, which is the freshness
//! invariant #842 established: journals change only by append or whole-file
//! rewrite, and either moves length or mtime, so an unchanged stamp set
//! proves a fresh fold would produce the same state. There is deliberately
//! no time-based or once-per-process shelf here — that is the seed-cache
//! failure mode (#842 round 3), and a watermark carried by the sender keeps
//! hit and fresh-fold answers indistinguishable (#855).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use serde_json::Value;

/// One journal's change detector: on-disk length and mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JournalStamp {
    len: u64,
    modified_nanos: u64,
}

impl JournalStamp {
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let nanos = meta
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos() as u64;
        Some(Self {
            len: meta.len(),
            modified_nanos: nanos,
        })
    }
}

/// Everything the fold reads, captured without opening a journal.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct BaselineFingerprint {
    active: String,
    members: BTreeMap<String, JournalStamp>,
}

struct CachedBaseline {
    fingerprint: BaselineFingerprint,
    state: Value,
}

/// `(sessions root, thread, channel)` — the fold's coordinates.
type BaselineKey = (String, String, String);
type BaselineCache = Mutex<HashMap<BaselineKey, CachedBaseline>>;

fn cache() -> &'static BaselineCache {
    static CACHE: OnceLock<BaselineCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Stamp the fold's input set: `active` plus one stamp per member journal.
/// A journal that vanished between enumeration and stamping is simply
/// absent — its absence moves the fingerprint the same way a moved stamp
/// does, and the fold skips it identically.
pub(super) fn fingerprint(active: &str, members: &[String]) -> BaselineFingerprint {
    let mut stamped = BTreeMap::new();
    for id in members {
        let Some(path) = crate::paths::persisted_session_file(id) else {
            continue;
        };
        if let Some(stamp) = JournalStamp::of(&path) {
            stamped.insert(id.clone(), stamp);
        }
    }
    BaselineFingerprint {
        active: active.to_string(),
        members: stamped,
    }
}

/// The cached fold for `(root, thread, channel)` when the journals' stamps
/// still match the fold that produced it.
pub(super) fn hit(
    root: &str,
    thread: &str,
    channel: &str,
    stamp: &BaselineFingerprint,
) -> Option<Value> {
    let entry = cache().lock().unwrap();
    entry
        .get(&(root.to_string(), thread.to_string(), channel.to_string()))
        .filter(|cached| &cached.fingerprint == stamp)
        .map(|cached| cached.state.clone())
}

/// Record the fresh fold for `(root, thread, channel)`, replacing any prior
/// entry for the same key (one entry per key: the superseded fold is dead
/// once a journal moved, mirroring `ConversationInfoCache`'s retention).
pub(super) fn store(
    root: &str,
    thread: &str,
    channel: &str,
    stamp: BaselineFingerprint,
    state: Value,
) {
    cache().lock().unwrap().insert(
        (root.to_string(), thread.to_string(), channel.to_string()),
        CachedBaseline {
            fingerprint: stamp,
            state,
        },
    );
}

/// Hit/fold counts, for the freshness tests that must observe the cache
/// actually answering (a pass without them would also pass with the cache
/// disabled). Outside tests this compiles to a no-op: the probe is
/// observability for the tests, not production telemetry.
#[cfg(test)]
fn counts() -> &'static Mutex<(u64, u64)> {
    static PROBE: OnceLock<Mutex<(u64, u64)>> = OnceLock::new();
    PROBE.get_or_init(|| Mutex::new((0, 0)))
}

#[cfg(test)]
pub(super) fn probe() -> (u64, u64) {
    *counts().lock().unwrap()
}

#[cfg(test)]
pub(super) fn note(hit: bool) {
    let mut slot = counts().lock().unwrap();
    if hit {
        slot.0 += 1;
    } else {
        slot.1 += 1;
    }
}

#[cfg(not(test))]
pub(super) fn note(_hit: bool) {}
