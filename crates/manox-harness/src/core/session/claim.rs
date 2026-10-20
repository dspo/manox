// The claim decision for `createSession`.
//
// Creation is idempotent-by-reuse, never redirecting: the answer names the
// session the client will drive, and that id never changes afterward. The
// decision runs on durable facts alone (the caller gathers them from the
// live table, the journal's header line, and the store); a
// never-materialized blank lives only in its creating process, where reuse
// is trivially the same handle.
//
// The whole table, in order:
//
//   untaken id        -> Mint(desired): the client drives the id it minted
//   subagent lineage  -> refused: the id belongs to its parent's run
//   archived          -> Mint(fresh): a retired identity is never reused
//   blank             -> Reuse: the client's own not-yet-used id
//   other directory   -> Conflict: explicit, never a silent redirect
//   otherwise         -> Reuse: a retry drives the session it already owns

use serde::{Deserialize, Serialize};

/// What the client asked for. `desired_id` is the idempotency key: the id
/// the client minted and intends to drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_id: Option<String>,
    /// The directory the client claims the session for. Absent = the client
    /// names no directory, so the stored session's origin directory is not
    /// challenged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// The durable facts about `desired_id`, gathered by the caller. `blank`
/// means the log carries no interaction-marking event — the same rule the
/// sidebar uses to hide blank rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFacts {
    pub id: String,
    /// A session is stored under this id (header published, or live).
    pub exists: bool,
    pub blank: bool,
    pub archived: bool,
    pub subagent: bool,
    /// The header's origin cwd, when the session is stored.
    pub origin_cwd: Option<String>,
}

/// What `createSession` does with the request. Every arm is terminal —
/// there is no follow-up step that changes a session's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// Drive the desired id: it holds a blank session.
    Reuse { id: String },
    /// Nothing reusable is stored under the id. Carries the desired id
    /// when it was never taken (the caller drives it), `None` when the
    /// stored session is non-reusable and the caller must mint a fresh one.
    Mint { id: Option<String> },
    /// The stored session lives in another directory. An explicit
    /// conflict: never a silent redirect, never a quiet fresh session.
    Conflict {
        id: String,
        requested_cwd: String,
        existing_cwd: String,
    },
    /// Subagent sessions belong to their parent's run; claiming one is
    /// refused outright.
    SubagentOwned { id: String },
}

/// Decide a claim. Pure over the gathered facts.
pub fn decide(request: &ClaimRequest, facts: &SessionFacts) -> ClaimOutcome {
    if !facts.exists {
        return ClaimOutcome::Mint {
            id: request.desired_id.clone(),
        };
    }
    if facts.subagent {
        return ClaimOutcome::SubagentOwned {
            id: facts.id.clone(),
        };
    }
    if facts.archived {
        return ClaimOutcome::Mint { id: None };
    }
    if facts.blank {
        return ClaimOutcome::Reuse {
            id: facts.id.clone(),
        };
    }
    if let (Some(requested), Some(existing)) = (request.cwd.as_deref(), facts.origin_cwd.as_deref())
        && existing != requested
    {
        return ClaimOutcome::Conflict {
            id: facts.id.clone(),
            requested_cwd: requested.to_string(),
            existing_cwd: existing.to_string(),
        };
    }
    // The same directory claims the same session again: a retry of the
    // client's own create (a lost answer, a reconnect) drives the session it
    // already owns instead of minting a second log for one intent.
    ClaimOutcome::Reuse {
        id: facts.id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(desired_id: Option<&str>, cwd: Option<&str>) -> ClaimRequest {
        ClaimRequest {
            desired_id: desired_id.map(str::to_string),
            cwd: cwd.map(str::to_string),
        }
    }

    fn facts(id: &str, exists: bool, blank: bool, archived: bool) -> SessionFacts {
        SessionFacts {
            id: id.into(),
            exists,
            blank,
            archived,
            subagent: false,
            origin_cwd: exists.then(|| "/w/a".into()),
        }
    }

    #[test]
    fn an_untaken_id_is_driven_as_desired() {
        let outcome = decide(
            &request(Some("s-9"), Some("/w/a")),
            &facts("s-9", false, true, false),
        );
        assert_eq!(
            outcome,
            ClaimOutcome::Mint {
                id: Some("s-9".into())
            }
        );
    }

    #[test]
    fn a_live_blank_is_reused_under_the_same_id() {
        let outcome = decide(
            &request(Some("s-1"), Some("/w/a")),
            &facts("s-1", true, true, false),
        );
        assert_eq!(outcome, ClaimOutcome::Reuse { id: "s-1".into() });
    }

    #[test]
    fn a_used_session_in_the_same_cwd_is_driven_again() {
        // A retry of the client's own create (a lost answer, a reconnect)
        // must drive the session it already owns: minting a second log for
        // one intent is the double-row accident this decision exists to
        // prevent.
        let outcome = decide(
            &request(Some("s-1"), Some("/w/a")),
            &facts("s-1", true, false, false),
        );
        assert_eq!(outcome, ClaimOutcome::Reuse { id: "s-1".into() });
    }

    #[test]
    fn a_claim_without_a_directory_never_conflicts() {
        let outcome = decide(
            &request(Some("s-1"), None),
            &facts("s-1", true, false, false),
        );
        assert_eq!(outcome, ClaimOutcome::Reuse { id: "s-1".into() });
    }

    #[test]
    fn a_materialized_blank_is_reused_wherever_the_claim_points() {
        // A crash between header publication and the first batch leaves a
        // materialized blank: no interaction landed, so the claim continues
        // the client's id and the header's origin cwd keeps deciding the
        // bucket alone.
        let outcome = decide(
            &request(Some("s-1"), Some("/w/b")),
            &facts("s-1", true, true, false),
        );
        assert_eq!(outcome, ClaimOutcome::Reuse { id: "s-1".into() });
    }

    #[test]
    fn a_used_session_in_another_cwd_is_an_explicit_conflict() {
        let outcome = decide(
            &request(Some("s-1"), Some("/w/b")),
            &facts("s-1", true, false, false),
        );
        assert_eq!(
            outcome,
            ClaimOutcome::Conflict {
                id: "s-1".into(),
                requested_cwd: "/w/b".into(),
                existing_cwd: "/w/a".into(),
            }
        );
    }

    #[test]
    fn an_archived_session_is_never_reused() {
        let outcome = decide(
            &request(Some("s-1"), Some("/w/a")),
            &facts("s-1", true, true, true),
        );
        assert_eq!(outcome, ClaimOutcome::Mint { id: None });
    }

    #[test]
    fn a_subagent_session_cannot_be_claimed() {
        let subagent = SessionFacts {
            subagent: true,
            ..facts("sub-1", true, true, false)
        };
        let outcome = decide(&request(Some("sub-1"), Some("/w/a")), &subagent);
        assert_eq!(outcome, ClaimOutcome::SubagentOwned { id: "sub-1".into() });
    }

    #[test]
    fn a_blank_request_without_a_desired_id_mints() {
        let outcome = decide(&request(None, Some("/w/a")), &facts("", false, true, false));
        assert_eq!(outcome, ClaimOutcome::Mint { id: None });
    }
}
