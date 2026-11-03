//! The summary `_meta` contract: the recency stamp in the host's reserved
//! seat inside a session summary's metadata.
//!
//! AHP session summaries carry an optional `_meta` object — the protocol's
//! explicitly server-defined metadata slot ("Lightweight server-defined
//! metadata clients may use for the session presentation"). The slot exists
//! precisely because the `ahp`/`ahp-types` crates are the protocol's
//! official vocabulary and cannot grow manox-specific typed fields, so
//! manox-owned presentation facts ride `_meta` under [`NAMESPACE`] — the one
//! `x-manox` seat ([`crate::ext::META_KEY`]) this host writes, shared with
//! the catalogue's pin and container facts — where no other producer can
//! collide with them.
//!
//! `lastMessageAt` — the one fact this contract carries today — is the
//! session's objective recency key for ordering surfaces. Clients rank a
//! recency-ordered surface by it, newest first; the host itself never
//! reorders the sidebar's durable account (that order stays the user's):
//!
//! - **Presence**: the key exists if and only if the session's transcript
//!   contains at least one message entry (the bounded scan's `has_messages`
//!   fact; the scalar scan counts any message role, in practice user or
//!   assistant). A session that never exchanged a message has no recency
//!   story; consumers filter it out of recency-ordered surfaces rather than
//!   ranking it.
//! - **Value**: unix seconds (integer) of the last user/assistant message,
//!   carried as the later of the sidecar's two durable stamps — the last
//!   human interaction (`interacted_at`) and the last settled turn
//!   (`last_settled_at`). A transcript whose last turn never settled carries
//!   only the interaction stamp, which can precede its last assistant
//!   message: the key is a recency approximation, not a message-time oracle.
//! - **Absence**: the transcript has no message entry, the producer predates
//!   this contract, or the surface is the session channel's mirrored summary
//!   (see below). Absence is not "very old" — it is "not rankable": do not
//!   default it to an epoch.
//! - **Legacy degradation**: a session whose sidecar carries neither stamp
//!   still reports the scan's mtime fallback as the interaction stamp, so
//!   its key degrades to "last durable write". This self-heals: the
//!   session's next real interaction stamps the sidecar and the key moves
//!   to its true value.
//!
//! Only catalogue summaries carry the key — `listSessions`, `sessionAdded`,
//! `sessionSummaryChanged`. The root channel's session-state mirror
//! ([`crate::channels::session::summary`]) deliberately publishes
//! `_meta: None`, so absence there says nothing about the transcript; read
//! the catalogue row when ranking.

use ahp_types::common::JsonObject;

/// The `_meta` seat this host reserves for its own presentation facts — the
/// same `x-manox` object that carries the catalogue's pin/container facts.
pub const NAMESPACE: &str = crate::ext::META_KEY;

/// The namespace key holding the unix-seconds recency stamp.
pub const LAST_MESSAGE_AT_KEY: &str = "lastMessageAt";

/// Write the recency stamp into a summary's `_meta` object. Other keys
/// inside the [`NAMESPACE`] object — and every other namespace — are
/// preserved; a non-object value squatting on the namespace is repaired
/// (the namespace is reserved, so nothing legitimate can be lost).
pub fn set_last_message_at(meta: &mut JsonObject, at: i64) {
    let namespace = meta
        .entry(NAMESPACE.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !namespace.is_object() {
        *namespace = serde_json::Value::Object(serde_json::Map::new());
    }
    namespace
        .as_object_mut()
        .expect("the namespace was just ensured to be an object")
        .insert(LAST_MESSAGE_AT_KEY.to_string(), serde_json::json!(at));
}

/// Read the recency stamp out of a summary's `_meta` object. `None` is the
/// contract's absence case (see the module doc): a transcript without
/// message entries — or a producer that never set the key.
pub fn last_message_at(meta: &JsonObject) -> Option<i64> {
    meta.get(NAMESPACE)?
        .as_object()?
        .get(LAST_MESSAGE_AT_KEY)?
        .as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_seat_is_the_host_extension_namespace() {
        assert_eq!(NAMESPACE, crate::ext::META_KEY);
    }

    #[test]
    fn set_then_read_round_trips() {
        let mut meta = JsonObject::new();
        set_last_message_at(&mut meta, 1_700_000_000);
        assert_eq!(last_message_at(&meta), Some(1_700_000_000));
    }

    #[test]
    fn set_moves_the_value_and_preserves_sibling_facts_and_namespaces() {
        let mut meta = JsonObject::new();
        meta.insert(NAMESPACE.to_string(), json!({ "pinned": true }));
        meta.insert("other".to_string(), json!({ "keep": 1 }));
        set_last_message_at(&mut meta, 100);
        set_last_message_at(&mut meta, 200);
        assert_eq!(last_message_at(&meta), Some(200), "an update overwrites");
        assert_eq!(
            meta[NAMESPACE]["pinned"],
            json!(true),
            "a sibling fact in the shared seat survives"
        );
        assert_eq!(
            meta["other"]["keep"],
            json!(1),
            "every other namespace survives"
        );
        assert_eq!(
            meta[NAMESPACE][LAST_MESSAGE_AT_KEY],
            json!(200),
            "the stamp is the only key the writer touches"
        );
    }

    #[test]
    fn a_squatting_non_object_namespace_is_repaired() {
        let mut meta = JsonObject::new();
        meta.insert(NAMESPACE.to_string(), json!("junk"));
        assert_eq!(last_message_at(&meta), None);
        set_last_message_at(&mut meta, 7);
        assert_eq!(last_message_at(&meta), Some(7));
    }

    #[test]
    fn missing_or_mistyped_facts_read_as_absent() {
        let mut meta = JsonObject::new();
        assert_eq!(last_message_at(&meta), None, "no namespace at all");
        meta.insert(NAMESPACE.to_string(), json!({ "other": 1 }));
        assert_eq!(last_message_at(&meta), None, "no stamp key");
        meta[NAMESPACE][LAST_MESSAGE_AT_KEY] = json!("not-a-number");
        assert_eq!(last_message_at(&meta), None, "a mistyped stamp");
    }
}
