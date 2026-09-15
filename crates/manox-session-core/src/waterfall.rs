//! ServerCall waterfall state machine (architecture §D.4): the multi-client
//! adjudication semantics, scoped by answer kind.
//!
//! One request fans out to every `owner(session) ∩ capability` client;
//! settlement is fan-in and the policy is kind-scoped:
//!
//! - [`SettlePolicy::Unanimous`] (`Approve`): **all** recipients must answer
//!   *next* for the kernel to proceed, **any** *rejected* (or lapsed)
//!   settles immediately and every remaining delivery is cancelled —
//!   fail-closed, never silently waiting forever.
//! - [`SettlePolicy::FirstClaim`] (`AskUserQuestion`): one answer is one
//!   truth — the FIRST non-Reseated reply settles the waterfall
//!   (an answer allows, a rejection or an explicit lapse fails closed),
//!   and every recipient still waiting is marked `Cancelled`: the gateway
//!   owes each of them a [`ServerNote::DeliveryCancelled`] frame so their
//!   card retires as "answered elsewhere", not as a lapse. A lapse only
//!   settles when no deliverable recipient is left to claim first.
//!
//! [`ServerNote::DeliveryCancelled`]: manox_protocol::ServerNote::DeliveryCancelled
//!
//! Single-recipient waterfalls degenerate to the current v1 semantics
//! (one owner answering), so the migration keeps behavior identical there.

use std::collections::BTreeMap;

/// How one adjudication settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaterfallOutcome {
    /// Every recipient answered next (Unanimous), or the first claim
    /// answered (FirstClaim).
    Allowed,
    /// A recipient rejected (by client id) or a delivery expired.
    Rejected { by: Option<String> },
}

/// The kind-scoped settle policy (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlePolicy {
    /// All must answer next; any rejection/expiry settles fail-closed.
    Unanimous,
    /// The first non-Reseated answer settles; rejections fail-closed;
    /// a lapse settles only once no deliverable recipient remains.
    FirstClaim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryState {
    Waiting,
    Next,
    Rejected,
    Cancelled,
}

/// One in-flight waterfall.
pub struct Waterfall {
    pub session_id: String,
    policy: SettlePolicy,
    deliveries: BTreeMap<String, DeliveryState>,
    settled: Option<WaterfallOutcome>,
}

impl Waterfall {
    /// Fan out to `recipients` (client ids) under [`SettlePolicy::Unanimous`].
    /// Empty means nobody can answer: the caller treats that as fail-closed
    /// before constructing one.
    pub fn new(session_id: impl Into<String>, recipients: Vec<String>) -> Self {
        Self::with_policy(session_id, recipients, SettlePolicy::Unanimous)
    }

    /// Fan out to `recipients` under [`SettlePolicy::FirstClaim`] — the
    /// `AskUserQuestion` semantics.
    pub fn first_claim(session_id: impl Into<String>, recipients: Vec<String>) -> Self {
        Self::with_policy(session_id, recipients, SettlePolicy::FirstClaim)
    }

    pub fn policy(&self) -> SettlePolicy {
        self.policy
    }

    fn with_policy(
        session_id: impl Into<String>,
        recipients: Vec<String>,
        policy: SettlePolicy,
    ) -> Self {
        Waterfall {
            session_id: session_id.into(),
            policy,
            deliveries: recipients
                .into_iter()
                .map(|id| (id, DeliveryState::Waiting))
                .collect(),
            settled: None,
        }
    }

    /// Record one recipient's reply. Returns the outcome iff this reply
    /// settles the waterfall.
    pub fn reply(&mut self, client_id: &str, next: bool) -> Option<WaterfallOutcome> {
        if self.settled.is_some() {
            return None; // late reply after settlement
        }
        let Some(state) = self.deliveries.get_mut(client_id) else {
            return None; // not a recipient of this waterfall
        };
        match state {
            DeliveryState::Waiting => {
                *state = if next {
                    DeliveryState::Next
                } else {
                    DeliveryState::Rejected
                };
            }
            // Duplicate reply: first answer stands.
            _ => return None,
        }
        if !next {
            // An explicit rejection is the human saying no: fail-closed
            // under BOTH policies.
            return Some(self.settle(WaterfallOutcome::Rejected {
                by: Some(client_id.to_string()),
            }));
        }
        match self.policy {
            SettlePolicy::FirstClaim => Some(self.settle(WaterfallOutcome::Allowed)),
            SettlePolicy::Unanimous => {
                if self
                    .deliveries
                    .values()
                    .all(|s| matches!(s, DeliveryState::Next))
                {
                    return Some(self.settle(WaterfallOutcome::Allowed));
                }
                None
            }
        }
    }

    /// A delivery lapsed without a human action (a withdrawn delivery, a
    /// closed channel): fail-closed for [`SettlePolicy::Unanimous`] — the
    /// quorum can no longer be reached, and a timeout/withdrawal counts
    /// as a rejection (never silently waiting forever). Under
    /// [`SettlePolicy::FirstClaim`] the answer is kind-scoped to WHOEVER
    /// CLAIMS FIRST: a lapse only removes that recipient from the claim
    /// set, and settles once no waiting recipient is left to answer —
    /// by the last lapse (fail-closed with nothing to show for it).
    pub fn expire(&mut self, client_id: &str) -> Option<WaterfallOutcome> {
        if self.settled.is_some() {
            return None;
        }
        let removed = match self.deliveries.get_mut(client_id) {
            Some(state) if *state == DeliveryState::Waiting => {
                *state = DeliveryState::Rejected;
                true
            }
            _ => false, // already answered/withdrawn/expired: no-op
        };
        if !removed {
            return None;
        }
        match self.policy {
            SettlePolicy::Unanimous => Some(self.settle(WaterfallOutcome::Rejected {
                by: Some(client_id.to_string()),
            })),
            SettlePolicy::FirstClaim => {
                if self
                    .deliveries
                    .values()
                    .any(|s| matches!(s, DeliveryState::Waiting))
                {
                    None // another recipient can still claim first
                } else {
                    Some(self.settle(WaterfallOutcome::Rejected {
                        by: Some(client_id.to_string()),
                    }))
                }
            }
        }
    }

    /// Remove a recipient whose delivery was abandoned by the gateway
    /// itself (the §D.6 re-seat hand-off: the owner's connection was
    /// replaced, so this waterfall no longer owns that delivery's settle —
    /// the replayed waiter does). Removing a recipient cannot itself fail
    /// the adjudication; it never produces a `Cancelled` mark either — a
    /// hand-off is not a cancel. Under [`SettlePolicy::Unanimous`] it
    /// settles `Allowed` only when it completes the all-must-answer
    /// quorum of the recipients that remain; under
    /// [`SettlePolicy::FirstClaim`] it cannot settle at all (a removed
    /// recipient is not an answer — the survivors still owe a claim).
    pub fn abandon(&mut self, client_id: &str) -> Option<WaterfallOutcome> {
        if self.settled.is_some() {
            return None;
        }
        self.deliveries.remove(client_id)?;
        match self.policy {
            SettlePolicy::FirstClaim => None,
            SettlePolicy::Unanimous => {
                if !self.deliveries.is_empty()
                    && self
                        .deliveries
                        .values()
                        .all(|s| matches!(s, DeliveryState::Next))
                {
                    return Some(self.settle(WaterfallOutcome::Allowed));
                }
                None
            }
        }
    }

    /// The recipients still owed a cancel frame after settlement (those
    /// still waiting when the waterfall settled — `settle` re-marks them
    /// `Cancelled`, which is exactly this set). Under
    /// [`SettlePolicy::FirstClaim`] an Allowed settle reaches this arm too:
    /// a claim answered, the rest never will.
    pub fn cancelled_recipients(&self) -> Vec<String> {
        self.deliveries
            .iter()
            .filter(|(_, state)| matches!(state, DeliveryState::Cancelled))
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn settled(&self) -> Option<WaterfallOutcome> {
        self.settled.clone()
    }

    pub fn recipients(&self) -> Vec<String> {
        self.deliveries.keys().cloned().collect()
    }

    fn settle(&mut self, outcome: WaterfallOutcome) -> WaterfallOutcome {
        if self.settled.is_none() {
            if outcome == WaterfallOutcome::Allowed {
                for state in self.deliveries.values_mut() {
                    match self.policy {
                        // Unanimous Allowed is all-next by definition;
                        // FirstClaim Allowed leaves the never-claiming
                        // recipients Cancelled — they are owed the
                        // "answered elsewhere" frame.
                        SettlePolicy::Unanimous => *state = DeliveryState::Next,
                        SettlePolicy::FirstClaim => {
                            if *state == DeliveryState::Waiting {
                                *state = DeliveryState::Cancelled;
                            }
                        }
                    }
                }
            } else {
                for state in self.deliveries.values_mut() {
                    if *state == DeliveryState::Waiting {
                        *state = DeliveryState::Cancelled;
                    }
                }
            }
            self.settled = Some(outcome.clone());
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_recipient_degenerates_to_v1_semantics() {
        let mut w = Waterfall::new("s1", vec!["desktop".into()]);
        assert_eq!(w.reply("desktop", true), Some(WaterfallOutcome::Allowed));
        assert_eq!(w.settled(), Some(WaterfallOutcome::Allowed));
        // No cancels owed.
        assert!(w.cancelled_recipients().is_empty());
    }

    #[test]
    fn all_next_settles_allowed() {
        let mut w = Waterfall::new("s1", vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(w.reply("a", true), None);
        assert_eq!(w.reply("b", true), None);
        assert_eq!(w.reply("c", true), Some(WaterfallOutcome::Allowed));
        assert_eq!(w.settled(), Some(WaterfallOutcome::Allowed));
    }

    #[test]
    fn any_rejection_settles_and_cancels_the_rest() {
        let mut w = Waterfall::new("s1", vec!["a".into(), "b".into(), "c".into()]);
        w.reply("a", true);
        assert_eq!(
            w.reply("b", false),
            Some(WaterfallOutcome::Rejected {
                by: Some("b".into())
            })
        );
        // c is owed a cancel frame.
        assert_eq!(w.cancelled_recipients(), vec!["c".to_string()]);
        // Late replies are ignored; settled state stands.
        assert_eq!(w.reply("c", true), None);
        assert_eq!(
            w.settled(),
            Some(WaterfallOutcome::Rejected {
                by: Some("b".into())
            })
        );
    }

    #[test]
    fn duplicate_and_foreign_replies_are_ignored() {
        let mut w = Waterfall::new("s1", vec!["a".into(), "b".into()]);
        assert_eq!(w.reply("a", true), None);
        assert_eq!(w.reply("a", false), None, "first answer stands");
        assert_eq!(w.reply("zzz", false), None, "not a recipient");
        assert!(w.settled().is_none());
        assert_eq!(w.reply("b", true), Some(WaterfallOutcome::Allowed));
    }

    #[test]
    fn expiry_fails_closed() {
        let mut w = Waterfall::new("s1", vec!["a".into(), "b".into()]);
        w.reply("a", true);
        assert_eq!(
            w.expire("b"),
            Some(WaterfallOutcome::Rejected {
                by: Some("b".into())
            })
        );
        // Expiring an already-answered client is a no-op.
        let mut w2 = Waterfall::new("s1", vec!["a".into()]);
        w2.reply("a", true);
        assert_eq!(w2.expire("a"), None);
    }

    #[test]
    fn abandon_drops_from_the_quorum_never_rejects() {
        // Sole recipient abandoning: nothing to decide, no settle.
        let mut w = Waterfall::new("s1", vec!["a".to_string()]);
        assert_eq!(w.abandon("a"), None);
        assert!(w.settled().is_none());
        // Abandoning completes the quorum: the remaining Next answer
        // settles Allowed (the abandoned delivery must not veto it).
        let mut w = Waterfall::new("s1", vec!["a".to_string(), "b".to_string()]);
        w.reply("a", true);
        assert_eq!(w.abandon("b"), Some(WaterfallOutcome::Allowed));
        // A still-waiting third recipient keeps the quorum open.
        let mut w = Waterfall::new(
            "s1",
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        );
        w.reply("a", true);
        assert_eq!(w.abandon("b"), None);
        assert!(w.settled().is_none());
        assert_eq!(w.abandon("c"), Some(WaterfallOutcome::Allowed));
        // Foreign/late abandons are inert.
        assert_eq!(w.abandon("zzz"), None);
        assert_eq!(w.abandon("a"), None, "already settled");
    }

    // ── C2: `AskUserQuestion` first-claim-wins semantics. ─────────────────

    #[test]
    fn first_claim_settles_on_the_first_answer() {
        let mut w = Waterfall::first_claim("s1", vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(w.policy(), SettlePolicy::FirstClaim);
        assert_eq!(w.reply("a", true), Some(WaterfallOutcome::Allowed));
        // The never-claiming recipients are owed the cancel frame.
        assert_eq!(
            w.cancelled_recipients(),
            vec!["b".to_string(), "c".to_string()]
        );
        // Late replies after settlement are inert.
        assert_eq!(w.reply("b", true), None);
        assert_eq!(w.settled(), Some(WaterfallOutcome::Allowed));
    }

    #[test]
    fn first_claim_rejection_still_fails_closed_and_cancels_the_rest() {
        let mut w = Waterfall::first_claim("s1", vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(
            w.reply("b", false),
            Some(WaterfallOutcome::Rejected {
                by: Some("b".into())
            })
        );
        assert_eq!(
            w.cancelled_recipients(),
            vec!["a".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn first_claim_lapse_hands_over_not_fails_closed() {
        // A withdrawn delivery under first-claim is a REMOVED claimant,
        // not a rejection: the remaining owner can still answer.
        let mut w = Waterfall::first_claim("s1", vec!["a".into(), "b".into()]);
        assert_eq!(w.expire("a"), None, "b can still claim");
        assert!(w.settled().is_none());
        // The expired claimant never owes a cancel frame and cannot
        // double-settle.
        assert!(w.cancelled_recipients().is_empty());
        assert_eq!(w.reply("a", true), None, "expired delivery cannot answer");
        // The last remaining claimant's lapse settles fail-closed — with
        // nobody left to claim, the adjudication converges against the
        // call by the last lapse.
        assert_eq!(
            w.expire("b"),
            Some(WaterfallOutcome::Rejected {
                by: Some("b".into())
            })
        );
        // Both lapsed answers are `Rejected`, not `Cancelled`: nobody is
        // owed a cancel frame for a delivery that already lapsed.
        assert!(w.cancelled_recipients().is_empty());
    }

    #[test]
    fn first_claim_reseat_only_drops_the_waiter() {
        // The §D.6 hand-off under FirstSettle: a re-seat removes the
        // recipient WITHOUT settling (the replayed waiter owns the
        // claim); the hand-off is never a cancel frame for the rest.
        let mut w = Waterfall::first_claim("s1", vec!["a".into(), "b".into()]);
        assert_eq!(w.abandon("a"), None, "a hand-off settles nothing");
        assert!(w.settled().is_none());
        assert!(w.cancelled_recipients().is_empty(), "no cancel on hand-off");
        // The sole survivor's answer still settles the claim.
        assert_eq!(w.reply("b", true), Some(WaterfallOutcome::Allowed));
        // Sole-recipient hand-off: removing the last claimant never
        // rejects — the replayed waiter owns the settle.
        let mut w = Waterfall::first_claim("s1", vec!["a".into()]);
        assert_eq!(w.abandon("a"), None);
        assert!(w.settled().is_none());
    }

    #[test]
    fn unanimous_quorum_is_unchanged_by_the_first_claim_widening() {
        // The Approve (Unanimous) arms are regression-pinned here at the
        // constructor level: `new` defaults to Unanimous and the
        // all-must-answer rule still holds — first answer does NOT
        // settle, every rejection does.
        let mut w = Waterfall::new("s1", vec!["a".into(), "b".into()]);
        assert_eq!(w.policy(), SettlePolicy::Unanimous);
        assert_eq!(w.reply("a", true), None);
        assert_eq!(w.reply("b", true), Some(WaterfallOutcome::Allowed));
        assert!(w.cancelled_recipients().is_empty());
    }
}
