//! The annotations channel (`ahp-session:/<id>/annotations`) — one session's
//! durable user annotations, AHP 1.0's file-anchored review notes.
//!
//! State is the SDK's `AnnotationsState` (annotations keyed by id, each with
//! its entries); the durable authority is the kernel session journal's
//! `annotation_set` / `annotation_removed` rows, and the host's fold is the
//! SDK reducer applied to the projection of those rows. Writes arrive as
//! client dispatches on this channel, land as journal rows host-side, and
//! echo back through the same fold every subscriber runs.

use ahp_types::state::AnnotationsState;

/// `ahp-session:/<id>/annotations`.
pub fn uri(session_id: &str) -> String {
    format!("ahp-session:/{session_id}/annotations")
}

/// The session id inside an annotations URI.
pub fn id(uri: &str) -> Option<&str> {
    uri.strip_prefix("ahp-session:/")?
        .strip_suffix("/annotations")
        .filter(|id| !id.is_empty())
}

/// An empty annotations channel.
pub fn initial() -> AnnotationsState {
    AnnotationsState {
        annotations: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uri_round_trips_through_its_session_id() {
        assert_eq!(uri("s-1"), "ahp-session:/s-1/annotations");
        assert_eq!(id("ahp-session:/s-1/annotations"), Some("s-1"));
        assert_eq!(
            id("ahp-session:/s-1"),
            None,
            "the plain session channel is not annotations"
        );
        assert_eq!(id("x-manox-thread:/s-1/annotations"), None);
    }
}
