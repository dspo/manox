//! The summary `_meta` contract: the manox-reserved namespace inside a
//! session summary's host metadata.
//!
//! AHP session summaries carry an optional `_meta` object — the protocol's
//! explicitly server-defined metadata slot ("Lightweight server-defined
//! metadata clients may use for the session presentation"). The slot exists
//! precisely because the `ahp`/`ahp-types` crates are the protocol's
//! official vocabulary and cannot grow manox-specific typed fields, so
//! manox-owned presentation facts ride `_meta` under the [`NAMESPACE`] key
//! where no other producer can collide with them.
//!
//! `lastMessageAt` — the one fact this contract carries today — is the
//! session's objective recency key for ordering surfaces (the catalogue
//! ranks sessions by it, newest first):
//!
//! - **Presence**: the key exists if and only if the session's transcript
//!   contains at least one user or assistant message (the bounded scan's
//!   `has_messages` fact). A session that never exchanged a message has no
//!   recency story; consumers filter it out of recency-ordered surfaces
//!   rather than ranking it.
//! - **Value**: unix seconds (integer) of the last user/assistant message,
//!   carried as the later of the sidecar's two durable stamps — the last
//!   human interaction (`interacted_at`) and the last settled turn
//!   (`last_settled_at`). Assistant output alone never advances it beyond
//!   the settle stamp.
//! - **Absence**: the transcript has no user/assistant message (or the
//!   producer predates this contract). Absence is not "very old" — it is
//!   "not rankable": do not default it to an epoch.
//! - **Legacy degradation**: a session whose sidecar carries neither stamp
//!   still reports the scan's mtime fallback as the interaction stamp, so
//!   its key degrades to "last durable write". This self-heals: the
//!   session's next real interaction stamps the sidecar and the key moves
//!   to its true value.

use ahp_types::common::JsonObject;

/// The `_meta` key this host reserves for its own presentation facts.
pub const NAMESPACE: &str = "manox";

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
/// user/assistant messages — or a producer that never set the key.
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
    fn set_then_read_round_trips() {
        let mut meta = JsonObject::new();
        set_last_message_at(&mut meta, 1_700_000_000);
        assert_eq!(last_message_at(&meta), Some(1_700_000_000));
    }

    #[test]
    fn set_moves_the_value_and_preserves_sibling_keys_and_namespaces() {
        let mut meta = JsonObject::new();
        meta.insert("x-manox".to_string(), json!({ "pinned": true }));
        set_last_message_at(&mut meta, 100);
        set_last_message_at(&mut meta, 200);
        assert_eq!(last_message_at(&meta), Some(200), "an update overwrites");
        assert_eq!(
            meta["x-manox"]["pinned"],
            json!(true),
            "a sibling namespace survives"
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
