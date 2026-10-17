// The claim decision for `createSession`.
//
// Creation is idempotent-by-reuse, never redirecting: the answer names the
// session the client will drive, and that id never changes afterward. The
// decision runs on durable facts alone (the caller gathers them from the
// store and the live registry); a never-materialized blank lives only in
// its creating process, where reuse is trivially the same handle.

use serde::{Deserialize, Serialize};

/// What the client asked for. `desired_id` is the idempotency key: the id
/// the client minted and intends to drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_id: Option<String>,
    pub cwd: String,
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
    if let Some(existing) = facts.origin_cwd.as_deref()
        && existing != request.cwd
    {
        return ClaimOutcome::Conflict {
            id: facts.id.clone(),
            requested_cwd: request.cwd.clone(),
            existing_cwd: existing.to_string(),
        };
    }
    ClaimOutcome::Mint { id: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(desired_id: Option<&str>, cwd: &str) -> ClaimRequest {
        ClaimRequest {
            desired_id: desired_id.map(str::to_string),
            cwd: cwd.into(),
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
            &request(Some("s-9"), "/w/a"),
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
            &request(Some("s-1"), "/w/a"),
            &facts("s-1", true, true, false),
        );
        assert_eq!(outcome, ClaimOutcome::Reuse { id: "s-1".into() });
    }

    #[test]
    fn a_used_session_in_the_same_cwd_mints_a_fresh_id() {
        let outcome = decide(
            &request(Some("s-1"), "/w/a"),
            &facts("s-1", true, false, false),
        );
        assert_eq!(outcome, ClaimOutcome::Mint { id: None });
    }

    #[test]
    fn a_used_session_in_another_cwd_is_an_explicit_conflict() {
        let outcome = decide(
            &request(Some("s-1"), "/w/b"),
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
            &request(Some("s-1"), "/w/a"),
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
        let outcome = decide(&request(Some("sub-1"), "/w/a"), &subagent);
        assert_eq!(outcome, ClaimOutcome::SubagentOwned { id: "sub-1".into() });
    }

    #[test]
    fn a_blank_request_without_a_desired_id_mints() {
        let outcome = decide(&request(None, "/w/a"), &facts("", false, true, false));
        assert_eq!(outcome, ClaimOutcome::Mint { id: None });
    }
}
