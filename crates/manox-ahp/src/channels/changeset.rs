//! The changeset channel (`ahp-changeset:/<session-id>/<key>`) — a
//! subscribable view of file changes. This host serves exactly one key per
//! session: `uncommitted` (see the runtime's changeset engine); a client
//! expanding any other template shape finds nothing here, which is the
//! honest answer for a capability the engine does not produce.

/// `ahp-changeset:/<session-id>/<key>`.
pub fn uri(session_id: &str, key: &str) -> String {
    format!("ahp-changeset:/{session_id}/{key}")
}

/// The (session id, changeset key) inside a changeset channel URI.
pub fn parse(uri: &str) -> Option<(String, &str)> {
    let rest = uri.strip_prefix("ahp-changeset:/")?;
    let (session_id, key) = rest.split_once('/')?;
    (!session_id.is_empty() && !key.is_empty()).then(|| (session_id.to_string(), key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_and_parse_round_trip() {
        let uri = uri("s-1", "uncommitted");
        assert_eq!(parse(&uri), Some(("s-1".to_string(), "uncommitted")));
        assert_eq!(parse("ahp-changeset:/s-1"), None);
        assert_eq!(parse("ahp-changeset://uncommitted"), None);
        assert_eq!(parse("ahp-session:/s-1"), None);
    }
}
